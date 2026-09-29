[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-test\.[1-9][0-9]*$')]
    [string]$CandidateLabel,

    [ValidateSet('Prepare', 'Publish')]
    [string]$Action = 'Prepare',

    [string]$Summary
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$candidate = [regex]::Match($CandidateLabel, '^(?<version>(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*))-test\.(?<number>[1-9][0-9]*)$')
if (-not $candidate.Success) { throw "Invalid candidate label '$CandidateLabel'." }
$version = $candidate.Groups['version'].Value
$tag = "v$version"
$tagRef = "refs/tags/$tag"
$candidatePath = Join-Path $repoRoot "dist\candidates\$CandidateLabel"
$candidateInfoPath = Join-Path $candidatePath 'build.json'
$releaseRoot = Join-Path $repoRoot 'dist\releases'
$releasePath = Join-Path $releaseRoot $tag
$releaseInfoPath = Join-Path $releasePath 'release.json'
$giteaRemote = 'origin'
$githubRemote = 'github'
$githubRepo = 'colthreepv/scribetray'

function Invoke-GitText {
    param([Parameter(Mandatory = $true)][string[]]$GitArguments)
    $result = & git -C $repoRoot @GitArguments
    if ($LASTEXITCODE -ne 0) { throw "git $($GitArguments -join ' ') failed." }
    return (($result -join [Environment]::NewLine).Trim())
}

function Get-RemoteTagCommit {
    param(
        [Parameter(Mandatory = $true)][string]$Remote,
        [Parameter(Mandatory = $true)][string]$Ref
    )
    $peeledRef = "$Ref^{}"
    $lines = & git -C $repoRoot ls-remote --tags $Remote $Ref $peeledRef
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect tag '$Ref' on remote '$Remote'." }
    foreach ($line in $lines) {
        $parts = ([string]$line) -split "`t", 2
        if ($parts.Count -eq 2 -and $parts[1] -eq $peeledRef) { return $parts[0] }
    }
    foreach ($line in $lines) {
        $parts = ([string]$line) -split "`t", 2
        if ($parts.Count -eq 2 -and $parts[1] -eq $Ref) { return $parts[0] }
    }
    throw "Tag '$Ref' is not present on remote '$Remote'."
}

if (-not (Test-Path -LiteralPath $candidateInfoPath -PathType Leaf)) {
    throw "Candidate record is missing: '$candidateInfoPath'."
}
$candidateInfo = Get-Content -LiteralPath $candidateInfoPath -Raw | ConvertFrom-Json
if ($candidateInfo.application -ne 'Scribetray' -or
    $candidateInfo.candidateLabel -ne $CandidateLabel -or
    $candidateInfo.version -ne $version -or
    $candidateInfo.sourceTreeClean -ne $true) {
    throw 'The candidate record does not identify a clean Scribetray build for this stable version.'
}
$candidateExe = Join-Path $candidatePath 'scribetray.exe'
if (-not (Test-Path -LiteralPath $candidateExe -PathType Leaf)) { throw "Candidate executable is missing: '$candidateExe'." }
$candidateHash = (Get-FileHash -LiteralPath $candidateExe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($candidateHash -ne ([string]$candidateInfo.executableSha256).ToLowerInvariant()) {
    throw 'The candidate executable no longer matches its recorded SHA-256.'
}

$repoTop = Invoke-GitText -GitArguments @('rev-parse', '--show-toplevel')
if ([IO.Path]::GetFullPath($repoTop) -ne [IO.Path]::GetFullPath($repoRoot)) {
    throw 'The release script is not running from the repository root.'
}
$cargoMetadataOutput = & cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $repoRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
$cargoMetadata = ($cargoMetadataOutput -join [Environment]::NewLine) | ConvertFrom-Json
$cargoPackage = $cargoMetadata.packages | Where-Object { $_.name -eq 'scribetray' } | Select-Object -First 1
if (-not $cargoPackage -or $cargoPackage.version -ne $version) {
    throw "Cargo.toml must remain at stable version $version for this release."
}

$localTagCommit = Invoke-GitText -GitArguments @('rev-parse', '--verify', "$tagRef^{commit}")
if ($localTagCommit -ne $candidateInfo.sourceCommit) {
    throw "Tag '$tag' does not point to the tested candidate commit."
}
$giteaTagCommit = Get-RemoteTagCommit -Remote $giteaRemote -Ref $tagRef
if ($giteaTagCommit -ne $localTagCommit) { throw "Gitea tag '$tag' does not match the tested commit." }
$githubTagCommit = Get-RemoteTagCommit -Remote $githubRemote -Ref $tagRef
if ($githubTagCommit -ne $localTagCommit) { throw "GitHub mirror tag '$tag' does not match the tested commit." }

$exeName = 'scribetray.exe'
$zipName = "scribetray-v$version-x86_64.zip"
$checksumsName = 'SHA256SUMS.txt'
$notesName = 'RELEASE_NOTES.md'
$exePath = Join-Path $releasePath $exeName
$zipPath = Join-Path $releasePath $zipName
$checksumsPath = Join-Path $releasePath $checksumsName
$notesPath = Join-Path $releasePath $notesName

if ($Action -eq 'Prepare') {
    if ([string]::IsNullOrWhiteSpace($Summary)) { throw 'Prepare requires a concise, agent-written release summary.' }
    if (Test-Path -LiteralPath $releasePath) {
        throw "Release package already exists at '$releasePath'; inspect it rather than overwriting approved files."
    }

    $templatePath = Join-Path $repoRoot 'docs\release-notes-template.md'
    if (-not (Test-Path -LiteralPath $templatePath -PathType Leaf)) { throw "Release notes template is missing: '$templatePath'." }
    $notes = Get-Content -LiteralPath $templatePath -Raw
    if (-not $notes.Contains('{{VERSION}}') -or -not $notes.Contains('{{SUMMARY}}')) {
        throw 'The release notes template must contain {{VERSION}} and {{SUMMARY}}.'
    }
    $notes = $notes.Replace('{{VERSION}}', $version).Replace('{{SUMMARY}}', $Summary.Trim())
    if ($notes.Contains('{{')) { throw 'The rendered release notes still contain an unfilled template placeholder.' }

    [void][IO.Directory]::CreateDirectory($releaseRoot)
    $stagingPath = Join-Path $releaseRoot ".staging-$tag-$([Guid]::NewGuid().ToString('N'))"
    try {
        [void][IO.Directory]::CreateDirectory($stagingPath)
        $stagedExe = Join-Path $stagingPath $exeName
        Copy-Item -LiteralPath $candidateExe -Destination $stagedExe
        $stagedExeHash = (Get-FileHash -LiteralPath $stagedExe -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($stagedExeHash -ne $candidateHash) { throw 'The staged executable differs from the approved candidate.' }

        $stagedZip = Join-Path $stagingPath $zipName
        Compress-Archive -LiteralPath $stagedExe -DestinationPath $stagedZip -CompressionLevel Optimal
        $zipEntries = @(tar -tf $stagedZip)
        if ($LASTEXITCODE -ne 0 -or $zipEntries.Count -ne 1 -or $zipEntries[0] -ne $exeName) {
            throw 'The release ZIP must contain only scribetray.exe at its root.'
        }

        $exeSha = (Get-FileHash -LiteralPath $stagedExe -Algorithm SHA256).Hash.ToLowerInvariant()
        $zipSha = (Get-FileHash -LiteralPath $stagedZip -Algorithm SHA256).Hash.ToLowerInvariant()
        $checksumLines = @("$exeSha  $exeName", "$zipSha  $zipName")
        [IO.File]::WriteAllLines((Join-Path $stagingPath $checksumsName), $checksumLines, [Text.UTF8Encoding]::new($false))
        [IO.File]::WriteAllText((Join-Path $stagingPath $notesName), $notes, [Text.UTF8Encoding]::new($false))

        $releaseInfo = [ordered]@{
            application = 'Scribetray'
            version = $version
            tag = $tag
            candidateLabel = $CandidateLabel
            sourceCommit = $localTagCommit
            executableSha256 = $exeSha
            zipSha256 = $zipSha
            preparedAtUtc = [DateTime]::UtcNow.ToString('o')
        }
        [IO.File]::WriteAllText((Join-Path $stagingPath 'release.json'), ($releaseInfo | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
        [IO.Directory]::Move($stagingPath, $releasePath)
    } finally {
        if (Test-Path -LiteralPath $stagingPath) { Remove-Item -LiteralPath $stagingPath -Recurse -Force }
    }

    Write-Host "Prepared release files at '$releasePath'. Review '$notesPath', then rerun with -Action Publish."
    return
}

if (-not (Test-Path -LiteralPath $releaseInfoPath -PathType Leaf)) {
    throw "Prepared release metadata is missing: '$releaseInfoPath'. Run with -Action Prepare first."
}
$releaseInfo = Get-Content -LiteralPath $releaseInfoPath -Raw | ConvertFrom-Json
if ($releaseInfo.version -ne $version -or
    $releaseInfo.tag -ne $tag -or
    $releaseInfo.candidateLabel -ne $CandidateLabel -or
    $releaseInfo.sourceCommit -ne $localTagCommit) {
    throw 'Prepared release metadata does not match this candidate and tag.'
}
foreach ($path in @($exePath, $zipPath, $checksumsPath, $notesPath)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Prepared release file is missing: '$path'." }
}
if ((Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $releaseInfo.executableSha256 -or
    (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne $releaseInfo.zipSha256) {
    throw 'Prepared release files no longer match their recorded checksums.'
}
$releaseNotes = Get-Content -LiteralPath $notesPath -Raw
if ($releaseNotes.Contains('{{') -or [string]::IsNullOrWhiteSpace($releaseNotes)) {
    throw 'Release notes are empty or contain unfilled template placeholders.'
}

$ghLogin = & gh api user --jq .login
if ($LASTEXITCODE -ne 0) { throw 'Could not verify the authenticated GitHub account.' }
if (([string]$ghLogin).Trim() -ne 'colthreepv') { throw "Expected GitHub account 'colthreepv', found '$ghLogin'." }
& gh release create $tag $exePath $zipPath $checksumsPath --repo $githubRepo --verify-tag --title "Scribetray $tag" --notes-file $notesPath
if ($LASTEXITCODE -ne 0) { throw "GitHub release creation failed for $tag." }
Write-Host "Published https://github.com/$githubRepo/releases/tag/$tag"
