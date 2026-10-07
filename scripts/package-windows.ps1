param(
    [string]$BinaryDirectory = '',
    [string]$OutputDirectory = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
if (!$BinaryDirectory) { $BinaryDirectory = Join-Path $root 'target/x86_64-pc-windows-msvc/release' }
if (!$OutputDirectory) { $OutputDirectory = Join-Path $root 'dist' }
$NodeVersion = '22.22.0'
$pinnedNodeSha256 = 'c97fa376d2becdc8863fcd3ca2dd9a83a9f3468ee7ccf7a6d076ec66a645c77a'
$sourceCommit = (& git -C $root rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0) { throw '无法确定源码提交。' }
$sourceTree = (& git -C $root rev-parse 'HEAD^{tree}').Trim()
if ($LASTEXITCODE -ne 0) { throw '无法确定源码树。' }
$name = "Eve-windows-x64-$($sourceCommit.Substring(0, 7))"
$bundle = Join-Path $OutputDirectory $name
if (Test-Path -LiteralPath $bundle) { throw '输出目录已存在；请选择新的输出目录，避免覆盖已验收发行包。' }
New-Item -ItemType Directory -Path $bundle -Force | Out-Null
foreach ($binary in @('eve', 'eve-qqbot', 'eve-cognition', 'eve-memory', 'eve-message-evaluate')) {
    Copy-Item -LiteralPath (Join-Path $BinaryDirectory "$binary.exe") -Destination $bundle
}
Copy-Item -LiteralPath (Join-Path $root 'AGENT.md') -Destination $bundle
Get-ChildItem -LiteralPath (Join-Path $root 'packaging/windows') -File | ForEach-Object {
    $destination = Join-Path $bundle $_.Name
    if ($_.Extension -eq '.ps1') {
        # Windows PowerShell 5.1 needs BOM to interpret Chinese source correctly.
        [IO.File]::WriteAllText($destination, [IO.File]::ReadAllText($_.FullName), [Text.UTF8Encoding]::new($true))
    } elseif ($_.Extension -eq '.cmd') {
        $body = [IO.File]::ReadAllText($_.FullName).Replace("`r`n", "`n").Replace("`n", "`r`n")
        [IO.File]::WriteAllText($destination, $body, [Text.ASCIIEncoding]::new())
    } else { Copy-Item -LiteralPath $_.FullName -Destination $destination }
}
$bridge = Join-Path $bundle 'connectors/qqbot'
New-Item -ItemType Directory -Path $bridge -Force | Out-Null
foreach ($file in @('bridge.mjs', 'bridge-core.mjs', 'package.json', 'package-lock.json', 'THIRD_PARTY.md')) {
    Copy-Item -LiteralPath (Join-Path $root "connectors/qqbot/$file") -Destination $bridge
}
Push-Location $bridge
try {
    & npm.cmd ci --omit=dev --ignore-scripts --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) { throw '安装锁定 QQ 桥接依赖失败。' }
} finally { Pop-Location }
$runtime = Join-Path $bundle 'runtime'
New-Item -ItemType Directory -Path $runtime -Force | Out-Null
$download = Join-Path $OutputDirectory "node-download-$NodeVersion"
New-Item -ItemType Directory -Path $download -Force | Out-Null
$nodeArchive = "node-v$NodeVersion-win-x64.zip"
$nodeZip = Join-Path $download $nodeArchive
$checksums = Join-Path $download 'SHASUMS256.txt'
Invoke-WebRequest "https://nodejs.org/dist/v$NodeVersion/SHASUMS256.txt" -OutFile $checksums
Invoke-WebRequest "https://nodejs.org/dist/v$NodeVersion/$nodeArchive" -OutFile $nodeZip
$checksumLines = @(Get-Content -LiteralPath $checksums | Where-Object { $_ -match "^[0-9a-f]{64}\s+$([regex]::Escape($nodeArchive))$" })
if ($checksumLines.Count -ne 1) { throw '官方 Node 校验清单缺少唯一 ZIP 项。' }
$expected = ($checksumLines[0] -split '\s+')[0]
if ($expected -cne $pinnedNodeSha256 -or (Get-FileHash -LiteralPath $nodeZip -Algorithm SHA256).Hash.ToLowerInvariant() -cne $pinnedNodeSha256) { throw 'Node 下载 SHA-256 不符。' }
Expand-Archive -LiteralPath $nodeZip -DestinationPath $download
$nodeRoot = Join-Path $download "node-v$NodeVersion-win-x64"
Copy-Item -LiteralPath (Join-Path $nodeRoot 'node.exe') -Destination $runtime
Copy-Item -LiteralPath (Join-Path $nodeRoot 'LICENSE') -Destination (Join-Path $runtime 'NODE-LICENSE.txt')
$nodeReportedVersion = & (Join-Path $runtime 'node.exe') --version
if ($LASTEXITCODE -ne 0 -or $nodeReportedVersion -cne "v$NodeVersion") { throw '打包 Node 版本不符。' }
foreach ($binary in @('eve', 'eve-qqbot', 'eve-cognition', 'eve-memory', 'eve-message-evaluate')) {
    & (Join-Path $bundle "$binary.exe") --help | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "$binary Windows 启动失败。" }
}
Push-Location $bridge
try {
    & (Join-Path $runtime 'node.exe') --input-type=module -e 'await import("@tencent-connect/qqbot-nodejs");'
    if ($LASTEXITCODE -ne 0) { throw '发行包内 QQ SDK 导入失败。' }
} finally { Pop-Location }
$files = @{}
Get-ChildItem -LiteralPath $bundle -File -Recurse | Sort-Object FullName | ForEach-Object {
    $relative = [IO.Path]::GetRelativePath($bundle, $_.FullName).Replace('\', '/')
    $files[$relative] = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
}
$manifest = [ordered]@{
    format_version = 1; source_commit = $sourceCommit; source_tree = $sourceTree
    target = 'x86_64-pc-windows-msvc'; rust_version = '1.89.0'; static_crt = $true
    node_version = $NodeVersion; node_archive_sha256 = $expected
    credentials_included = $false; user_data_included = $false; files_sha256 = $files
}
[IO.File]::WriteAllText((Join-Path $bundle 'build-info.json'), ($manifest | ConvertTo-Json -Depth 6), [Text.UTF8Encoding]::new($false))
$zipPath = Join-Path $OutputDirectory "$name.zip"
Compress-Archive -LiteralPath $bundle -DestinationPath $zipPath
$zipHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $OutputDirectory 'SHA256SUMS.txt'), "$zipHash  $name.zip`n", [Text.UTF8Encoding]::new($false))
Write-Host "发行包已生成：$zipPath"
