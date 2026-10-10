param(
    [ValidateSet('qq', 'console', 'update')][string]$Mode = 'qq',
    [string]$ConfigPath = '',
    [switch]$NoPrompt,
    [switch]$ValidateOnly,
    [switch]$NoUpdate,
    [switch]$NoCheckUpdates,
    [switch]$Rollback,
    [switch]$Status,
    [string]$BridgeScript = '',
    [string]$BridgeArg = ''
)
$ErrorActionPreference = 'Stop'
$node = Join-Path $PSScriptRoot 'runtime/node.exe'
$launcher = Join-Path $PSScriptRoot 'Launch.mjs'
$arguments = @($launcher, $Mode)
if ($ConfigPath) { $arguments += @('--config-path', $ConfigPath) }
if ($NoPrompt) { $arguments += '--no-prompt' }
if ($ValidateOnly) { $arguments += '--validate-only' }
if ($NoUpdate) { $arguments += '--no-update' }
if ($NoCheckUpdates) { $arguments += '--no-check-updates' }
if ($Rollback) { $arguments += '--rollback' }
if ($Status) { $arguments += '--status' }
if ($BridgeScript) { $arguments += @('--bridge-script', $BridgeScript) }
if ($BridgeArg) { $arguments += @('--bridge-arg', $BridgeArg) }
try {
    if (!(Test-Path -LiteralPath $node -PathType Leaf)) { throw '随包 Node 不存在。' }
    $ErrorActionPreference = 'Continue'
    & $node @arguments
    exit $LASTEXITCODE
} catch {
    Write-Host 'Eve 启动器无法运行，请检查完整解压的运行包。'
    exit 1
}
