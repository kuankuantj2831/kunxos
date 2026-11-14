# Build with the installed Rust cross target and open native Windows QEMU.
param([switch]$NoBuild)
$ErrorActionPreference = 'Stop'
$kunxRunScript = Join-Path $PSScriptRoot 'run_windows.py'
if ($NoBuild) {
    & python $kunxRunScript
} else {
    & python $kunxRunScript --build
}
exit $LASTEXITCODE
