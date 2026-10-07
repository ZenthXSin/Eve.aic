$ErrorActionPreference = 'Stop'
try {
    try { $config = Get-Content -LiteralPath (Join-Path $PSScriptRoot 'config.json') -Raw -Encoding UTF8 | ConvertFrom-Json }
    catch { throw 'config.json 无法读取或不是有效 JSON，请先运行 Start-Eve.cmd。' }
    if ($config.web.listen -notmatch '^127\.0\.0\.1:([0-9]{1,5})$' -or [int]$Matches[1] -lt 1 -or [int]$Matches[1] -gt 65535) {
        throw '面板地址无效；端口 0 请使用 EVE_WEB_READY 输出的实际地址。'
    }
    if ($config.web.token -notmatch '^[\x21-\x7e]{32,256}$') { throw '请先运行 Start-Eve.cmd 完成配置。' }
    Write-Host '请复制以下本机面板登录令牌，在浏览器登录框粘贴：'
    Write-Host $config.web.token
    Start-Process "http://$($config.web.listen)"
} catch { Write-Error $_ -ErrorAction Continue; exit 1 }
