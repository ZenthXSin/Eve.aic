// 更新定义：只处理公开稳定发行、平台选择与文件清单，不做网络或文件写入。
export const REPOSITORY = 'ZenthXSin/Eve.aic';
export const PROTOCOL = 1;
export const MAX_ARCHIVE_BYTES = 160 * 1024 * 1024;
export const MAX_UNPACKED_BYTES = 512 * 1024 * 1024;
export const MAX_FILES = 12000;
export const TARGETS = {
  'windows-x64': 'x86_64-pc-windows-msvc', 'windows-arm64': 'aarch64-pc-windows-msvc',
  'linux-x64': 'x86_64-unknown-linux-gnu', 'linux-arm64': 'aarch64-unknown-linux-gnu',
  'macos-x64': 'x86_64-apple-darwin', 'macos-arm64': 'aarch64-apple-darwin',
};

export function stableVersion(value) {
  const match = /^v(0|[1-9]\d{0,8})\.(0|[1-9]\d{0,8})\.(0|[1-9]\d{0,8})$/.exec(value ?? '');
  return match ? match.slice(1).map(Number) : null;
}
export function compareVersions(a, b) {
  const first = stableVersion(a), second = stableVersion(b);
  if (!first || !second) throw new Error('更新版本格式无效。');
  for (let i = 0; i < 3; i++) if (first[i] !== second[i]) return Math.sign(first[i] - second[i]);
  return 0;
}
export function safeRelative(value) {
  if (typeof value !== 'string' || !value || value.length > 1024 || /[\\:\x00-\x1f\x7f]/.test(value)) throw new Error('更新包路径无效。');
  const parts = value.split('/');
  if (parts.some(part => !part || part === '.' || part === '..' || /[. ]$/.test(part) || /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(part))) throw new Error('更新包路径越界或不兼容。');
  return value;
}
export function currentManifest(value) {
  if (!value || value.format_version !== 1 || !Object.hasOwn(TARGETS, value.platform_label) || TARGETS[value.platform_label] !== value.target || !stableVersion(value.version)) throw new Error('当前运行包没有有效稳定发行信息。');
  if (value.updater_protocol !== PROTOCOL || value.state_schema_generation !== 1) throw new Error('运行包更新协议不兼容。');
  return value;
}
export function updatePlan(release, current) {
  currentManifest(current);
  if (!release || release.draft || release.prerelease || !stableVersion(release.tag_name)) throw new Error('最新发行不是公开稳定版本。');
  if (compareVersions(release.tag_name, current.version) <= 0) return null;
  const extension = current.platform_label.startsWith('windows-') ? '.zip' : '.tar.gz';
  const name = `Eve-${release.tag_name}-${current.platform_label}${extension}`;
  const asset = release.assets?.find(item => item.name === name);
  const digest = /^sha256:([a-f0-9]{64})$/.exec(asset?.digest ?? '');
  const url = `https://github.com/${REPOSITORY}/releases/download/${release.tag_name}/${name}`;
  if (!asset || asset.browser_download_url !== url || !digest || !Number.isSafeInteger(asset.size) || asset.size < 1 || asset.size > MAX_ARCHIVE_BYTES) throw new Error('新版缺少相符平台的已校验资产。');
  return { version: release.tag_name, label: current.platform_label, target: current.target,
    name, url, sha256: digest[1], size: asset.size, folder: name.slice(0, -extension.length) };
}
export function verifyManifest(manifest, plan, names) {
  currentManifest(manifest);
  if (manifest.version !== plan.version || manifest.target !== plan.target || manifest.platform_label !== plan.label ||
      !/^[a-f0-9]{40}$/.test(manifest.source_commit ?? '') || !/^[a-f0-9]{40}$/.test(manifest.source_tree ?? '') ||
      manifest.credentials_included !== false || manifest.user_data_included !== false) throw new Error('新版发行信息不符。');
  const entries = Object.entries(manifest.files_sha256 ?? {});
  if (!entries.length || entries.length > MAX_FILES) throw new Error('更新文件清单无效。');
  const seen = new Map();
  for (const [name, digest] of entries) {
    safeRelative(name);
    if (!/^[a-f0-9]{64}$/.test(digest) || /^(?:config\.json|data|\.eve-updates)(?:\/|$)/i.test(name) || name === 'build-info.json') throw new Error('更新清单包含无效文件或用户数据。');
    const parts = name.split('/');
    for (let i = 1; i <= parts.length; i++) {
      const part = parts.slice(0, i).join('/'), folded = part.toLowerCase();
      if (seen.has(folded) && seen.get(folded) !== part) throw new Error('更新路径大小写冲突。');
      seen.set(folded, part);
    }
  }
  for (const [name] of entries) {
    const parts = name.split('/');
    for (let i = 1; i < parts.length; i++) if (Object.hasOwn(manifest.files_sha256, parts.slice(0, i).join('/'))) throw new Error('更新文件和目录冲突。');
  }
  const extension = plan.label.startsWith('windows-') ? '.exe' : '';
  const required = ['Launch.mjs', 'Updater.mjs', 'update-contract.mjs', 'update-archive.mjs', 'AGENT.md', 'config.example.json',
    `runtime/${extension ? 'node.exe' : 'bin/node'}`, 'connectors/qqbot/bridge.mjs',
    'connectors/qqbot/node_modules/@tencent-connect/qqbot-nodejs/package.json',
    ...['eve', 'eve-qqbot', 'eve-cognition', 'eve-memory', 'eve-message-evaluate'].map(name => name + extension)];
  if (required.some(name => !Object.hasOwn(manifest.files_sha256, name))) throw new Error('新版运行包不完整。');
  if (names && (names.length !== entries.length + 1 || names.some(name => name !== 'build-info.json' && !Object.hasOwn(manifest.files_sha256, name)))) throw new Error('更新包与文件清单不一致。');
  return entries;
}
