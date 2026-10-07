param(
    [string]$BinaryDirectory = '',
    [string]$OutputDirectory = '',
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc'
)
$ErrorActionPreference = 'Stop'
$arguments = @((Join-Path $PSScriptRoot 'package-portable.py'), '--target', $Target)
if ($BinaryDirectory) { $arguments += @('--binary-directory', $BinaryDirectory) }
if ($OutputDirectory) { $arguments += @('--output-directory', $OutputDirectory) }
& python @arguments
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
