# Dot-source before Cargo commands to locate wreq/BoringSSL build tools.
# This script does not install software or change persistent environment variables.
$ErrorActionPreference = 'Stop'

function Find-ScribetrayTool {
    param([string]$Override, [string]$Name, [string]$Fallback)
    if ($Override) {
        if (-not (Test-Path -LiteralPath $Override -PathType Leaf)) {
            throw "Tool does not exist: $Override"
        }
        return [IO.Path]::GetFullPath($Override)
    }
    $command = Get-Command $Name -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($command) { return $command.Source }
    if (Test-Path -LiteralPath $Fallback -PathType Leaf) { return $Fallback }
    throw "Install $Name and add it to PATH, or set its documented environment override. See docs/development.md."
}

$scribetrayNasm = Find-ScribetrayTool $env:CMAKE_ASM_NASM_COMPILER 'nasm.exe' (Join-Path $env:USERPROFILE 'bin\nasm\nasm.exe')
$scribetrayCmake = Find-ScribetrayTool $env:CMAKE 'cmake.exe' (Join-Path $env:USERPROFILE 'bin\cmake\bin\cmake.exe')
$scribetrayClang = if ($env:LIBCLANG_PATH) { $env:LIBCLANG_PATH } else { Join-Path $env:USERPROFILE 'bin\libclang' }
if (-not (Test-Path -LiteralPath (Join-Path $scribetrayClang 'libclang.dll') -PathType Leaf)) {
    throw 'Set LIBCLANG_PATH to the directory containing libclang.dll. See docs/development.md.'
}
$env:CMAKE_ASM_NASM_COMPILER = $scribetrayNasm
$env:CMAKE = $scribetrayCmake
$env:LIBCLANG_PATH = $scribetrayClang
$env:PATH = "$(Split-Path -Parent $scribetrayNasm);$env:PATH"
