param(
    [ValidateSet('qq', 'console')][string]$Mode = 'qq',
    [string]$ConfigPath = '',
    [switch]$NoPrompt,
    [switch]$ValidateOnly,
    [string]$BridgeScript = '',
    [string]$BridgeArg = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Read-Secret([string]$Label) {
    $secure = Read-Host $Label -AsSecureString
    $pointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($pointer) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($pointer); $secure.Dispose() }
}

function New-WebToken {
    $bytes = New-Object byte[] 32
    $random = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $random.GetBytes($bytes) } finally { $random.Dispose() }
    return ([BitConverter]::ToString($bytes)).Replace('-', '').ToLowerInvariant()
}

$previous = @{}
$exitCode = 1
$oldLocation = Get-Location
try {
    Set-Location -LiteralPath $PSScriptRoot
    if (!$ConfigPath) { $ConfigPath = Join-Path $PSScriptRoot 'config.json' }
    if (Test-Path -LiteralPath $ConfigPath) {
        try { $config = Get-Content -LiteralPath $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json }
        catch { throw 'config.json 无法读取或不是有效 JSON；原文件未修改，请检查配置。' }
    } else {
        if ($NoPrompt -or $ValidateOnly) { throw '缺少 config.json，请先运行 Start-Eve.cmd 完成配置。' }
        $config = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'config.example.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    }
    if ($config.format_version -ne 1) { throw '配置版本不支持。' }
    $changed = $false
    if (!$NoPrompt -and !$ValidateOnly) {
        if ([string]::IsNullOrWhiteSpace($config.model.api_key)) {
            $config.model.api_key = Read-Secret '请输入主模型 API 密钥（输入不显示）'
            $changed = $true
        }
        if ($Mode -eq 'qq' -and [string]::IsNullOrWhiteSpace($config.qq.app_secret)) {
            $config.qq.app_secret = Read-Secret '请输入 QQ App Secret（输入不显示）'
            $changed = $true
        }
        if ($Mode -eq 'qq' -and $config.jev.enabled -and [string]::IsNullOrWhiteSpace($config.jev.api_key)) {
            $config.jev.api_key = Read-Secret '请输入 OpenRouter / Jev API 密钥（输入不显示）'
            $changed = $true
        }
        if ($Mode -eq 'qq' -and [string]::IsNullOrWhiteSpace($config.web.token)) {
            $config.web.token = New-WebToken
            $changed = $true
        }
    }
    if ([string]::IsNullOrWhiteSpace($config.model.api_key)) { throw '主模型 API 密钥为空。' }
    if ($config.model.protocol -notin @('chat', 'responses')) { throw 'model.protocol 只能为 chat 或 responses。' }
    if ([string]::IsNullOrWhiteSpace($config.model.name)) { throw 'model.name 不能为空。' }
    foreach ($property in @('training', 'cognition', 'self_learning', 'memory_recall', 'segmented')) {
        if ($config.features.$property -isnot [bool]) { throw "features.$property 必须为 JSON 布尔值。" }
    }
    if ($Mode -eq 'qq') {
        if ($config.qq.app_id -notmatch '^[0-9]{1,32}$') { throw 'QQ AppID 格式错误。' }
        if ([string]::IsNullOrWhiteSpace($config.qq.app_secret)) { throw 'QQ App Secret 为空。' }
        if ($config.qq.sandbox -isnot [bool] -or $config.jev.enabled -isnot [bool]) { throw 'sandbox / jev.enabled 必须为 JSON 布尔值。' }
        if ($config.web.token -notmatch '^[\x21-\x7e]{32,256}$') { throw 'web.token 必须为 32 至 256 个可见 ASCII 字符。' }
        if ($config.web.listen -notmatch '^127\.0\.0\.1:([0-9]{1,5})$' -or [int]$Matches[1] -gt 65535) {
            throw 'web.listen 必须为 127.0.0.1:端口（0 至 65535）。'
        }
        if ($config.jev.enabled -and [string]::IsNullOrWhiteSpace($config.jev.api_key)) { throw '已启用 Jev，但未填写独立密钥。' }
    }
    $binary = Join-Path $PSScriptRoot $(if ($Mode -eq 'qq') { 'eve-qqbot.exe' } else { 'eve.exe' })
    $required = @($binary, (Join-Path $PSScriptRoot 'AGENT.md'))
    if ($Mode -eq 'qq') {
        $required += Join-Path $PSScriptRoot 'runtime/node.exe'
        if (!$BridgeScript) { $BridgeScript = Join-Path $PSScriptRoot 'connectors/qqbot/bridge.mjs' }
        $required += $BridgeScript
        $required += Join-Path $PSScriptRoot 'connectors/qqbot/node_modules/@tencent-connect/qqbot-nodejs/package.json'
    }
    foreach ($path in $required) { if (!(Test-Path -LiteralPath $path -PathType Leaf)) { throw "发行包文件缺失：$path" } }
    if ($ValidateOnly) { Write-Host '配置及发行包文件检查通过。'; exit 0 }
    if ($changed) {
        [IO.File]::WriteAllText($ConfigPath, ($config | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
        Write-Host '配置已保存到 config.json；密钥只用于本机进程，请保持此文件私有。'
    }
    $variables = @{
        EVE_OPENAI_API_KEY = $config.model.api_key
        EVE_OPENAI_BASE_URL = $config.model.base_url
        EVE_OPENAI_MODEL = $config.model.name
        EVE_OPENAI_PROTOCOL = $config.model.protocol
        EVE_OPENAI_MODEL_ROLE = ''
        EVE_OPENAI_REASONING_EFFORT = 'none'
        EVE_LLM_RESPONSE_MODE = 'complete'
    }
    $arguments = @('--state-dir', (Join-Path $PSScriptRoot "data/$Mode"), '--agent', (Join-Path $PSScriptRoot 'AGENT.md'))
    if ($config.features.segmented) { $arguments += '--segmented' }
    if ($Mode -eq 'qq') {
        $variables.QQBOT_APP_ID = $config.qq.app_id
        $variables.QQBOT_APP_SECRET = $config.qq.app_secret
        $variables.QQBOT_SANDBOX = $config.qq.sandbox.ToString().ToLowerInvariant()
        $variables.EVE_WEB_TOKEN = $config.web.token
        $variables.EVE_JEV_API_KEY = ''
        $arguments += @('--node', (Join-Path $PSScriptRoot 'runtime/node.exe'), '--bridge-script', $BridgeScript, '--memory', '--web-listen', $config.web.listen)
        if ($BridgeArg) { $arguments += @('--bridge-arg', $BridgeArg) }
        if ($config.features.training) { $arguments += '--training' }
        if ($config.features.cognition) { $arguments += '--cognition' }
        if ($config.features.self_learning) { $arguments += '--self-learning' }
        if ($config.features.memory_recall) { $arguments += '--memory-recall' }
        if ($config.jev.enabled) {
            $variables.EVE_JEV_API_KEY = $config.jev.api_key
            $variables.EVE_JEV_BASE_URL = $config.jev.base_url
            $variables.EVE_MODELS_JEV_ENABLED = 'true'
            $variables.EVE_MODELS_JEV_PROVIDER = 'jev'
            $variables.EVE_MODELS_JEV_MODEL = $config.jev.model
            $variables.EVE_MODELS_JEV_CREDENTIAL_REF = 'env:EVE_JEV_API_KEY'
            $variables.EVE_MODELS_JEV_TIMEOUT_MS = '3000'
            $variables.EVE_MODELS_JEV_MAX_CONCURRENT_REQUESTS = '4'
            $variables.EVE_MODELS_JEV_MAX_OUTPUT_TOKENS = '0'
            $variables.EVE_MESSAGE_JUDGE_TIMEOUT_MS = '6000'
            $arguments += @('--message-judge', 'jev')
        }
        Write-Host 'QQ 启动中。看到 EVE_QQBOT_READY / EVE_WEB_READY 后，双击 Open-Panel.cmd 打开面板。'
    }
    foreach ($key in $variables.Keys) {
        $previous[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
        [Environment]::SetEnvironmentVariable($key, [string]$variables[$key], 'Process')
    }
    $ErrorActionPreference = 'Continue'
    try { & $binary @arguments; $exitCode = $LASTEXITCODE }
    finally { $ErrorActionPreference = 'Stop' }
} catch {
    Write-Error $_ -ErrorAction Continue
} finally {
    foreach ($key in $previous.Keys) { [Environment]::SetEnvironmentVariable($key, $previous[$key], 'Process') }
    Set-Location -LiteralPath $oldLocation.Path
}
exit $exitCode
