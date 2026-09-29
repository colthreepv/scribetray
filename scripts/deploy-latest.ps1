[CmdletBinding()]
param(
    [string]$BuildRoot,
    [int]$KeepBuilds = 0,
    [string]$Target,
    [string[]]$CargoArgs = @(),
    [string]$CandidateLabel
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repoRoot 'Cargo.toml'
$configPath = Join-Path $env:APPDATA 'Scribetray\deploy.json'
$config = [pscustomobject]@{}

if (Test-Path -LiteralPath $configPath -PathType Leaf) {
    try {
        $config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
    } catch {
        throw "Could not read deployment settings at '$configPath': $($_.Exception.Message)"
    }
}

if (-not $PSBoundParameters.ContainsKey('BuildRoot')) {
    if ($env:SCRIBETRAY_BUILD_ROOT) { $BuildRoot = $env:SCRIBETRAY_BUILD_ROOT }
    elseif ($config.buildRoot) { $BuildRoot = [Environment]::ExpandEnvironmentVariables($config.buildRoot) }
    else { $BuildRoot = Join-Path $env:LOCALAPPDATA 'Scribetray\builds' }
}
if (-not $PSBoundParameters.ContainsKey('KeepBuilds')) {
    if ($env:SCRIBETRAY_KEEP_BUILDS) { $KeepBuilds = [int]$env:SCRIBETRAY_KEEP_BUILDS }
    elseif ($null -ne $config.keepBuilds) { $KeepBuilds = [int]$config.keepBuilds }
    else { $KeepBuilds = 3 }
}
if (-not $PSBoundParameters.ContainsKey('Target')) {
    if ($env:SCRIBETRAY_TARGET) { $Target = $env:SCRIBETRAY_TARGET }
    elseif ($config.target) { $Target = [string]$config.target }
}
if (-not $PSBoundParameters.ContainsKey('CargoArgs') -and $null -ne $config.cargoArgs) {
    $CargoArgs = @($config.cargoArgs | ForEach-Object { [string]$_ })
}

if ($KeepBuilds -lt 1) { throw 'KeepBuilds must be at least 1.' }
if (-not $BuildRoot) { throw 'BuildRoot cannot be empty.' }
$BuildRoot = [IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($BuildRoot))
[void][IO.Directory]::CreateDirectory($BuildRoot)

$metadataOutput = & cargo metadata --no-deps --format-version 1 --manifest-path $manifest
if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed with exit code $LASTEXITCODE." }
$cargoMetadata = ($metadataOutput -join [Environment]::NewLine) | ConvertFrom-Json
$package = $cargoMetadata.packages | Where-Object { $_.name -eq 'scribetray' } | Select-Object -First 1
if (-not $package) { throw 'Cargo metadata did not contain the scribetray package.' }

$sourceCommit = (& git -C $repoRoot rev-parse HEAD 2>$null | Select-Object -First 1)
if ($LASTEXITCODE -ne 0 -or -not $sourceCommit) { throw 'Could not identify the source commit.' }
$sourceCommit = ([string]$sourceCommit).Trim()
$sourceStatus = ((& git -C $repoRoot status --porcelain --untracked-files=all 2>$null) -join [Environment]::NewLine).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the source working tree.' }
$sourceTreeClean = [string]::IsNullOrWhiteSpace($sourceStatus)
if ($CandidateLabel) {
    $candidatePattern = '^' + [regex]::Escape([string]$package.version) + '-test\.[1-9][0-9]*$'
    if ($CandidateLabel -notmatch $candidatePattern) {
        throw "CandidateLabel must match '$($package.version)-test.N', with N starting at 1."
    }
    if (-not $sourceTreeClean) {
        throw 'A named test candidate requires a clean, committed working tree.'
    }
    $candidatePath = Join-Path $repoRoot "dist\candidates\$CandidateLabel"
    if (Test-Path -LiteralPath $candidatePath) {
        throw "Candidate '$CandidateLabel' already exists. Increment the test number instead of replacing it."
    }
}

$buildArgs = @('build', '--release', '--locked', '--manifest-path', $manifest)
if ($Target) { $buildArgs += @('--target', $Target) }
$buildArgs += $CargoArgs
Write-Host "Building Scribetray $($package.version)..."
& cargo @buildArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }

$builtCommit = (& git -C $repoRoot rev-parse HEAD 2>$null | Select-Object -First 1)
if ($LASTEXITCODE -ne 0 -or ([string]$builtCommit).Trim() -ne $sourceCommit) {
    throw 'The source commit changed while Cargo was building; refusing to deploy this artifact.'
}
$postBuildStatus = ((& git -C $repoRoot status --porcelain --untracked-files=all 2>$null) -join [Environment]::NewLine).Trim()
if ($LASTEXITCODE -ne 0) { throw 'Could not recheck the source working tree after building.' }
if ($CandidateLabel -and -not [string]::IsNullOrWhiteSpace($postBuildStatus)) {
    throw 'The source working tree changed while Cargo was building; refusing to deploy this candidate.'
}

$targetDirectory = [string]$cargoMetadata.target_directory
for ($i = 0; $i -lt $CargoArgs.Count; $i++) {
    if ($CargoArgs[$i] -eq '--target-dir' -and $i + 1 -lt $CargoArgs.Count) {
        $targetDirectory = $CargoArgs[$i + 1]
        if (-not [IO.Path]::IsPathRooted($targetDirectory)) { $targetDirectory = Join-Path $repoRoot $targetDirectory }
        break
    }
    if ($CargoArgs[$i] -like '--target-dir=*') {
        $targetDirectory = $CargoArgs[$i].Substring('--target-dir='.Length)
        if (-not [IO.Path]::IsPathRooted($targetDirectory)) { $targetDirectory = Join-Path $repoRoot $targetDirectory }
        break
    }
}
$artifactDirectory = if ($Target) { Join-Path $targetDirectory "$Target\release" } else { Join-Path $targetDirectory 'release' }
$artifact = Join-Path $artifactDirectory 'scribetray.exe'
if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) { throw "Cargo succeeded, but the executable was not found at '$artifact'." }

$stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffffffZ')
$buildLabel = if ($CandidateLabel) { $CandidateLabel } else { [string]$package.version }
$buildId = "build-$buildLabel-$stamp-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
$buildPath = Join-Path $BuildRoot $buildId
$stagingPath = Join-Path $BuildRoot ".staging-$([Guid]::NewGuid().ToString('N'))"
$candidateStagingPath = if ($CandidateLabel) {
    Join-Path (Join-Path $repoRoot 'dist\candidates') ".staging-$([Guid]::NewGuid().ToString('N'))"
} else { $null }
$latestPath = Join-Path $BuildRoot 'latest'
$nextLinkPath = Join-Path $BuildRoot ".latest-next-$([Guid]::NewGuid().ToString('N'))"
$oldLinkPath = Join-Path $BuildRoot ".latest-old-$([Guid]::NewGuid().ToString('N'))"
$previousLatestBuildId = $null

try {
    [void][IO.Directory]::CreateDirectory($stagingPath)
    Copy-Item -LiteralPath $artifact -Destination (Join-Path $stagingPath 'scribetray.exe')
    $artifactHash = (Get-FileHash -LiteralPath (Join-Path $stagingPath 'scribetray.exe') -Algorithm SHA256).Hash.ToLowerInvariant()
    $cargoVersion = (& cargo --version | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0) { throw 'Could not record the Cargo version.' }
    $rustcVersion = (& rustc --version --verbose | Out-String).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Could not record the Rust compiler version.' }
    $hostTarget = (& rustc --version --verbose | Where-Object { $_ -match '^host: ' } | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0) { throw 'Could not determine the Rust host target.' }
    $resolvedTarget = if ($Target) { $Target } elseif ($hostTarget) { ([string]$hostTarget -replace '^host: ', '').Trim() } else { 'host-default' }
    $buildInfo = [ordered]@{
        application = 'Scribetray'
        buildId = $buildId
        version = [string]$package.version
        candidateLabel = if ($CandidateLabel) { $CandidateLabel } else { $null }
        builtAtUtc = [DateTime]::UtcNow.ToString('o')
        sourceCommit = $sourceCommit
        sourceTreeClean = $sourceTreeClean
        target = $resolvedTarget
        cargoVersion = ([string]$cargoVersion).Trim()
        rustcVersion = $rustcVersion
        cargoArgs = @($CargoArgs)
        executableSha256 = $artifactHash
    }
    $json = $buildInfo | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $stagingPath 'build.json'), $json, [Text.UTF8Encoding]::new($false))
    [IO.Directory]::Move($stagingPath, $buildPath)

    if ($CandidateLabel) {
        $candidateRoot = Join-Path $repoRoot 'dist\candidates'
        [void][IO.Directory]::CreateDirectory($candidateRoot)
        [void][IO.Directory]::CreateDirectory($candidateStagingPath)
        Copy-Item -LiteralPath (Join-Path $buildPath 'scribetray.exe') -Destination (Join-Path $candidateStagingPath 'scribetray.exe')
        Copy-Item -LiteralPath (Join-Path $buildPath 'build.json') -Destination (Join-Path $candidateStagingPath 'build.json')
        [IO.Directory]::Move($candidateStagingPath, $candidatePath)
    }

    [void](New-Item -ItemType Junction -Path $nextLinkPath -Target $buildPath)
    $hadPrevious = Test-Path -LiteralPath $latestPath
    if ($hadPrevious) {
        $latestItem = Get-Item -LiteralPath $latestPath -Force
        if (($latestItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) {
            throw "Refusing to replace '$latestPath' because it is not a junction."
        }
        $previousInfo = Get-Content -LiteralPath (Join-Path $latestPath 'build.json') -Raw | ConvertFrom-Json
        $previousBuildPath = Join-Path $BuildRoot ([string]$previousInfo.buildId)
        if ($previousInfo.application -ne 'Scribetray' -or
            -not (Test-Path -LiteralPath (Join-Path $previousBuildPath 'build.json') -PathType Leaf)) {
            throw "Refusing to replace '$latestPath' because it does not point to a managed Scribetray build."
        }
        $previousLatestBuildId = [string]$previousInfo.buildId
        [IO.Directory]::Move($latestPath, $oldLinkPath)
    }

    try {
        [IO.Directory]::Move($nextLinkPath, $latestPath)
    } catch {
        if ($hadPrevious -and -not (Test-Path -LiteralPath $latestPath) -and (Test-Path -LiteralPath $oldLinkPath)) {
            [IO.Directory]::Move($oldLinkPath, $latestPath)
        }
        throw
    }
    if (Test-Path -LiteralPath $oldLinkPath) { [IO.Directory]::Delete($oldLinkPath) }
} finally {
    if (Test-Path -LiteralPath $stagingPath) { Remove-Item -LiteralPath $stagingPath -Recurse -Force }
    if ($candidateStagingPath -and (Test-Path -LiteralPath $candidateStagingPath)) {
        Remove-Item -LiteralPath $candidateStagingPath -Recurse -Force
    }
    if (Test-Path -LiteralPath $nextLinkPath) { [IO.Directory]::Delete($nextLinkPath) }
}

$latestInfo = Get-Content -LiteralPath (Join-Path $latestPath 'build.json') -Raw | ConvertFrom-Json
$runningPaths = @()
try {
    $runningPaths = @(Get-CimInstance Win32_Process -Filter "Name = 'scribetray.exe'" -ErrorAction Stop |
        ForEach-Object { $_.ExecutablePath } | Where-Object { $_ })
} catch {
    $runningPaths = @(Get-Process -Name scribetray -ErrorAction SilentlyContinue |
        ForEach-Object { try { $_.Path } catch { $null } } | Where-Object { $_ })
}
$runningPaths = @($runningPaths | ForEach-Object { [IO.Path]::GetFullPath($_) })
if ($previousLatestBuildId -and
    $runningPaths -contains [IO.Path]::GetFullPath((Join-Path $latestPath 'scribetray.exe'))) {
    # Windows may report the stable junction path instead of its versioned target.
    $runningPaths += [IO.Path]::GetFullPath((Join-Path (Join-Path $BuildRoot $previousLatestBuildId) 'scribetray.exe'))
}
$managedBuilds = @()
foreach ($directory in Get-ChildItem -LiteralPath $BuildRoot -Directory -Force) {
    if ($directory.Name -notlike 'build-*' -or
        ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { continue }
    $infoPath = Join-Path $directory.FullName 'build.json'
    if (-not (Test-Path -LiteralPath $infoPath -PathType Leaf)) { continue }
    try { $info = Get-Content -LiteralPath $infoPath -Raw | ConvertFrom-Json } catch { continue }
    if ($info.application -eq 'Scribetray' -and $info.buildId -eq $directory.Name) {
        $managedBuilds += [pscustomobject]@{ Path = $directory.FullName; Info = $info }
    }
}
$orderedBuilds = @($managedBuilds | Sort-Object { [string]$_.Info.builtAtUtc } -Descending)
$keepIds = @($orderedBuilds | Select-Object -First $KeepBuilds | ForEach-Object { $_.Info.buildId })
$keepIds += [string]$latestInfo.buildId
foreach ($build in $orderedBuilds) {
    if ($keepIds -contains [string]$build.Info.buildId) { continue }
    $exePath = [IO.Path]::GetFullPath((Join-Path $build.Path 'scribetray.exe'))
    if ($runningPaths -contains $exePath) {
        Write-Host "Keeping running build $($build.Info.version) ($($build.Info.buildId))."
        continue
    }
    try {
        Remove-Item -LiteralPath $build.Path -Recurse -Force
        Write-Host "Removed old build $($build.Info.buildId)."
    } catch {
        Write-Warning "Could not remove old build '$($build.Path)': $($_.Exception.Message)"
    }
}

if ($CandidateLabel) {
    Write-Host "Candidate $CandidateLabel is deployed at '$latestPath\scribetray.exe'."
    Write-Host "Candidate record and executable: '$candidatePath'."
    Write-Host 'Close any already-running Scribetray instance, then start the latest executable to test this candidate.'
} else {
    Write-Host "Scribetray $($package.version) is now available at '$latestPath\scribetray.exe'."
}
