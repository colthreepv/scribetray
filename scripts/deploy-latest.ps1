[CmdletBinding()]
param(
    [string]$BuildRoot,
    [int]$KeepBuilds = 0,
    [string]$Target,
    [string[]]$CargoArgs = @()
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

$buildArgs = @('build', '--release', '--locked', '--manifest-path', $manifest)
if ($Target) { $buildArgs += @('--target', $Target) }
$buildArgs += $CargoArgs
Write-Host "Building Scribetray $($package.version)..."
& cargo @buildArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE." }

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
$buildId = "build-$($package.version)-$stamp-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
$buildPath = Join-Path $BuildRoot $buildId
$stagingPath = Join-Path $BuildRoot ".staging-$([Guid]::NewGuid().ToString('N'))"
$latestPath = Join-Path $BuildRoot 'latest'
$nextLinkPath = Join-Path $BuildRoot ".latest-next-$([Guid]::NewGuid().ToString('N'))"
$oldLinkPath = Join-Path $BuildRoot ".latest-old-$([Guid]::NewGuid().ToString('N'))"
$previousLatestBuildId = $null

try {
    [void][IO.Directory]::CreateDirectory($stagingPath)
    Copy-Item -LiteralPath $artifact -Destination (Join-Path $stagingPath 'scribetray.exe')
    $buildInfo = [ordered]@{
        application = 'Scribetray'
        buildId = $buildId
        version = [string]$package.version
        builtAtUtc = [DateTime]::UtcNow.ToString('o')
    }
    $sourceCommit = (& git -C $repoRoot rev-parse HEAD 2>$null | Select-Object -First 1)
    if ($LASTEXITCODE -eq 0 -and $sourceCommit) { $buildInfo.sourceCommit = [string]$sourceCommit }
    $json = $buildInfo | ConvertTo-Json
    [IO.File]::WriteAllText((Join-Path $stagingPath 'build.json'), $json, [Text.UTF8Encoding]::new($false))
    [IO.Directory]::Move($stagingPath, $buildPath)

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

Write-Host "Scribetray $($package.version) is now available at '$latestPath\scribetray.exe'."
