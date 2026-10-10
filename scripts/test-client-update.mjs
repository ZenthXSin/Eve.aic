import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { gzipSync, crc32 } from 'node:zlib';
import { createUpdater, UpdateBusy } from '../packaging/common/Updater.mjs';
import { currentManifest, updatePlan, compareVersions, safeRelative, verifyManifest, TARGETS } from '../packaging/common/update-contract.mjs';
import { unpack } from '../packaging/common/update-archive.mjs';

const digest = value => createHash('sha256').update(value).digest('hex');
const base = { format_version: 1, version: 'v1.0.0', platform_label: 'linux-x64', target: TARGETS['linux-x64'],
  source_commit: 'a'.repeat(40), source_tree: 'b'.repeat(40), updater_protocol: 1, state_schema_generation: 1,
  credentials_included: false, user_data_included: false };
function tree(version = 'v1.1.0', label = 'linux-x64') {
  const extension = label.startsWith('windows-') ? '.exe' : '';
  const files = new Map(['Launch.mjs', 'Updater.mjs', 'update-contract.mjs', 'update-archive.mjs', 'AGENT.md', 'config.example.json',
    `runtime/${extension ? 'node.exe' : 'bin/node'}`, 'connectors/qqbot/bridge.mjs',
    'connectors/qqbot/node_modules/@tencent-connect/qqbot-nodejs/package.json',
    ...['eve', 'eve-qqbot', 'eve-cognition', 'eve-memory', 'eve-message-evaluate'].map(name => name + extension)]
    .map(name => [name, Buffer.from('fixture-' + name)]));
  files.set('使用说明.md', Buffer.from('合成发行说明'));
  const manifest = { ...base, version, platform_label: label, target: TARGETS[label], files_sha256: Object.fromEntries([...files].map(([name, bytes]) => [name, digest(bytes)])) };
  files.set('build-info.json', Buffer.from(JSON.stringify(manifest)));
  return { files, manifest };
}
function tarHeader(name, bytes, type = '0') {
  const header = Buffer.alloc(512), slash = name.lastIndexOf('/');
  const leaf = name.length > 90 ? name.slice(slash + 1) : name, prefix = name.length > 90 ? name.slice(0, slash) : '';
  header.write(leaf, 0, 100); header.write(prefix, 345, 155);
  const field = (offset, length, number) => header.write(number.toString(8).padStart(length - 1, '0') + '\0', offset, length);
  field(100, 8, 0o755); field(124, 12, bytes.length); field(136, 12, 0);
  header.fill(32, 148, 156); header[156] = type.charCodeAt(0); header.write('ustar\0', 257);
  const checksum = header.reduce((a, b) => a + b, 0); header.write(checksum.toString(8).padStart(6, '0') + '\0 ', 148, 8);
  return Buffer.concat([header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512)]);
}
function archiveTar(files, folder) {
  return gzipSync(Buffer.concat([...files].map(([name, bytes]) => tarHeader(folder + '/' + name, bytes)).concat([Buffer.alloc(1024)])));
}
function archiveZip(files, folder, mode = 0o100755) {
  const locals = [], centrals = []; let offset = 0;
  for (const [relative, bytes] of files) {
    const name = Buffer.from(folder + '/' + relative), local = Buffer.alloc(30), central = Buffer.alloc(46);
    local.writeUInt32LE(0x04034b50); local.writeUInt16LE(20, 4); local.writeUInt16LE(0x800, 6);
    local.writeUInt32LE(crc32(bytes), 14); local.writeUInt32LE(bytes.length, 18); local.writeUInt32LE(bytes.length, 22); local.writeUInt16LE(name.length, 26);
    central.writeUInt32LE(0x02014b50); central.writeUInt16LE(0x314, 4); central.writeUInt16LE(20, 6); central.writeUInt16LE(0x800, 8);
    central.writeUInt32LE(crc32(bytes), 16); central.writeUInt32LE(bytes.length, 20); central.writeUInt32LE(bytes.length, 24);
    central.writeUInt16LE(name.length, 28); central.writeUInt32LE((mode << 16) >>> 0, 38); central.writeUInt32LE(offset, 42);
    locals.push(local, name, bytes); centrals.push(central, name); offset += 30 + name.length + bytes.length;
  }
  const directory = Buffer.concat(centrals), end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50); end.writeUInt16LE(files.size, 8); end.writeUInt16LE(files.size, 10);
  end.writeUInt32LE(directory.length, 12); end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}
function releaseOf(archive, version = 'v1.1.0', label = 'linux-x64') {
  const name = `Eve-${version}-${label}` + (label.startsWith('windows-') ? '.zip' : '.tar.gz');
  return { tag_name: version, draft: false, prerelease: false, assets: [{ name, size: archive.length,
    digest: 'sha256:' + digest(archive), browser_download_url: `https://github.com/ZenthXSin/Eve.aic/releases/download/${version}/${name}` }] };
}
async function fixture(t, { brokenProbe = false } = {}) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'Eve 更新 空格 '));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.writeFile(path.join(root, 'build-info.json'), JSON.stringify(base));
  await fs.writeFile(path.join(root, 'config.json'), 'SYNTHETIC_PRIVATE_CONFIG');
  await fs.mkdir(path.join(root, 'data')); await fs.writeFile(path.join(root, 'data', 'training.json'), 'SYNTHETIC_TRAINING');
  const current = tree(); let archive = archiveTar(current.files, 'Eve-v1.1.0-linux-x64'), release = releaseOf(archive);
  const calls = [], probes = [], logs = [];
  const fetchImpl = async (url, options) => {
    calls.push({ url, options });
    return new Response(url.includes('/releases/latest') ? JSON.stringify(release) : archive);
  };
  const updater = createUpdater(root, { fetchImpl, log: text => logs.push(text), runProgram: async (...args) => {
    probes.push(args); if (brokenProbe) throw new Error('fixture probe failed');
  } });
  return { root, updater, calls, probes, logs, mutate(nextArchive, nextRelease) { archive = nextArchive; release = nextRelease; } };
}
async function preserved(f) {
  assert.equal(await fs.readFile(path.join(f.root, 'config.json'), 'utf8'), 'SYNTHETIC_PRIVATE_CONFIG');
  assert.equal(await fs.readFile(path.join(f.root, 'data', 'training.json'), 'utf8'), 'SYNTHETIC_TRAINING');
  assert(!f.logs.some(text => text.includes('SYNTHETIC_PRIVATE_CONFIG') || text.includes('SYNTHETIC_TRAINING')));
}

test('稳定版本按数值比较，拒绝预发行、降级、错误平台和外部下载地址', () => {
  assert.equal(compareVersions('v1.10.0', 'v1.9.9'), 1);
  const bytes = Buffer.from('archive'), release = releaseOf(bytes);
  assert.equal(updatePlan(release, base).label, 'linux-x64');
  assert.equal(updatePlan({ ...release, tag_name: 'v0.9.0' }, base), null);
  assert.throws(() => updatePlan({ ...release, prerelease: true }, base));
  assert.throws(() => compareVersions('v01.0.0', base.version));
  assert.throws(() => updatePlan({ ...release, assets: [{ ...release.assets[0], browser_download_url: 'https://example.com/evil' }] }, base));
  assert.throws(() => updatePlan({ ...release, assets: [] }, base));
});
test('六个平台均选用对应原生资产', () => {
  for (const platform_label of [undefined, 'unknown', '__proto__']) {
    assert.throws(() => currentManifest({ ...base, platform_label, target: undefined }));
  }
  for (const [label, target] of Object.entries(TARGETS)) {
    const release = releaseOf(Buffer.from('archive'), 'v2.0.0', label);
    assert.equal(updatePlan(release, { ...base, platform_label: label, target }).target, target);
  }
});
test('ZIP 与 tar 实际解码、中文路径和文件清单一致', () => {
  for (const label of ['linux-x64', 'windows-x64']) {
    const { files, manifest } = tree('v1.1.0', label), folder = 'Eve-v1.1.0-' + label;
    const archive = label.startsWith('windows-') ? archiveZip(files, folder) : archiveTar(files, folder);
    const plan = updatePlan(releaseOf(archive, 'v1.1.0', label), { ...base, platform_label: label, target: TARGETS[label] });
    const decoded = unpack(archive, plan);
    verifyManifest(manifest, plan, [...decoded.keys()]);
    assert.equal(decoded.get('使用说明.md').bytes.toString(), '合成发行说明');
  }
});
test('拒绝路径穿越、Windows 流和保留设备名', () => {
  for (const name of ['../config.json', '/absolute', 'a/../b', 'a\\b', 'a:b', 'CON.txt', 'x/nul', 'a.']) assert.throws(() => safeRelative(name));
  const files = new Map([['../config.json', Buffer.from('evil')]]), folder = 'Eve-v1.1.0-linux-x64';
  assert.throws(() => unpack(archiveTar(files, folder), { name: 'package.tar.gz', folder }));
  assert.throws(() => unpack(archiveZip(files, folder), { name: 'package.zip', folder }));
});
test('拒绝 tar 符号链接和 ZIP 特殊文件', () => {
  const folder = 'Eve-v1.1.0-linux-x64';
  const linked = gzipSync(Buffer.concat([tarHeader(folder + '/link', Buffer.alloc(0), '2'), Buffer.alloc(1024)]));
  assert.throws(() => unpack(linked, { name: 'package.tar.gz', folder }));
  assert.throws(() => unpack(archiveZip(new Map([['link', Buffer.from('outside')]]), folder, 0o120777), { name: 'package.zip', folder }));
});
test('清单拒绝用户数据、大小写冲突和状态代际不兼容', () => {
  const { manifest, files } = tree(), plan = { version: manifest.version, target: manifest.target, label: manifest.platform_label };
  for (const mutated of [
    { ...manifest, state_schema_generation: 2 }, { ...manifest, updater_protocol: 2 },
    { ...manifest, files_sha256: { ...manifest.files_sha256, 'config.json': digest('private') } },
    { ...manifest, files_sha256: { ...manifest.files_sha256, 'agent.md': digest('collision') } },
  ]) assert.throws(() => verifyManifest(mutated, plan));
  assert.throws(() => verifyManifest(manifest, plan, [...files.keys(), 'unlisted-file']));
});
test('下载、校验、下次启动切换与回退，用户文件字节不变', async t => {
  const f = await fixture(t); await f.updater.check({ force: true });
  let saved = await f.updater.status(); assert.equal(saved.active, null); assert.equal(saved.pending.version, 'v1.1.0');
  assert.equal(f.probes.length, 7); assert(f.probes.at(-1)[1].includes('--check'));
  const selected = await f.updater.resolveInstalled(); assert.equal(selected.manifest.version, 'v1.1.0');
  assert(f.probes.at(-1)[1].includes('--validate-only'));
  saved = await f.updater.status(); assert.equal(saved.active.version, 'v1.1.0'); assert.equal(saved.pending, null);
  await f.updater.rollback(); assert.equal((await f.updater.resolveInstalled()).appRoot, f.root);
  await preserved(f); assert.equal(f.calls.length, 2);
});
test('关闭本次更新不切换已下载版本', async t => {
  const f = await fixture(t); await f.updater.check({ force: true });
  assert.equal((await f.updater.resolveInstalled({ activate: false })).appRoot, f.root);
  assert.equal((await f.updater.status()).pending.version, 'v1.1.0'); await preserved(f);
});
test('第二次升级保留上一版本，显式回退恢复原选择', async t => {
  const f = await fixture(t); await f.updater.check({ force: true }); await f.updater.resolveInstalled();
  const next = tree('v1.2.0'), archive = archiveTar(next.files, 'Eve-v1.2.0-linux-x64');
  f.mutate(archive, releaseOf(archive, 'v1.2.0')); await f.updater.check({ force: true }); await f.updater.resolveInstalled();
  assert.equal((await f.updater.status()).previous.version, 'v1.1.0');
  await f.updater.rollback(); assert.equal((await f.updater.resolveInstalled()).manifest.version, 'v1.1.0'); await preserved(f);
});
test('损坏下载不成为待启用版本', async t => {
  const f = await fixture(t), archive = Buffer.from('corrupt');
  f.mutate(archive, { ...releaseOf(archive), assets: [{ ...releaseOf(archive).assets[0], digest: 'sha256:' + '0'.repeat(64) }] });
  await assert.rejects(f.updater.check({ force: true }));
  assert.equal((await f.updater.status()).pending, null); await preserved(f);
});
test('文件散列与实际内容不符时拒绝安装', async t => {
  const f = await fixture(t), candidate = tree(); candidate.files.set('eve', Buffer.from('changed executable'));
  const archive = archiveTar(candidate.files, 'Eve-v1.1.0-linux-x64'); f.mutate(archive, releaseOf(archive));
  await assert.rejects(f.updater.check({ force: true })); assert.equal((await f.updater.status()).pending, null); await preserved(f);
});
test('运行器或新版启动器自检失败时继续使用旧版', async t => {
  const f = await fixture(t, { brokenProbe: true }); await assert.rejects(f.updater.check({ force: true }));
  assert.equal((await f.updater.resolveInstalled()).appRoot, f.root); await preserved(f);
});
test('待启用目录损坏后拒绝切换，并保留损坏证据', async t => {
  const f = await fixture(t); await f.updater.check({ force: true }); const pending = (await f.updater.status()).pending;
  const executable = path.join(f.root, '.eve-updates/versions', pending.slot, 'eve'); await fs.writeFile(executable, 'tampered');
  assert.equal((await f.updater.resolveInstalled()).appRoot, f.root);
  assert.equal((await f.updater.status()).pending_failed, true); assert.equal(await fs.readFile(executable, 'utf8'), 'tampered'); await preserved(f);
});
test('损坏状态不清空；原字节保留', async t => {
  const f = await fixture(t); await fs.mkdir(path.join(f.root, '.eve-updates'));
  const file = path.join(f.root, '.eve-updates/installation.json'); await fs.writeFile(file, '{broken');
  await assert.rejects(f.updater.resolveInstalled()); assert.equal(await fs.readFile(file, 'utf8'), '{broken'); await preserved(f);
});
test('更新锁阻止并发下载；正常退出释放锁', async t => {
  const f = await fixture(t); let release;
  const updater = createUpdater(f.root, { fetchImpl: async () => {
    await new Promise(resolve => { release = resolve; }); throw new Error('synthetic offline');
  }, log: () => {} });
  const first = updater.check({ force: true }); const outcome = first.catch(error => error);
  while (!release) await new Promise(resolve => setTimeout(resolve, 5));
  await assert.rejects(f.updater.check({ force: true }), UpdateBusy);
  release(); await outcome; await assert.rejects(fs.access(path.join(f.root, '.eve-updates/update.lock'))); await preserved(f);
});
test('已退出进程的锁可恢复；已有完成下载可跨实例读取', async t => {
  const f = await fixture(t); await fs.mkdir(path.join(f.root, '.eve-updates'));
  await fs.writeFile(path.join(f.root, '.eve-updates/update.lock'), JSON.stringify({ pid: 99999999, protocol: 1 }));
  await f.updater.check({ force: true }); const reopened = createUpdater(f.root, { log: () => {}, runProgram: async () => {} });
  assert.equal((await reopened.resolveInstalled()).manifest.version, 'v1.1.0'); await preserved(f);
});
test('检查间隔内不空转请求，手动检查可立即重试', async t => {
  const f = await fixture(t); const bytes = Buffer.from('same'); f.mutate(bytes, releaseOf(bytes, 'v1.0.0'));
  await f.updater.check({ force: true }); await f.updater.check(); assert.equal(f.calls.length, 1);
  await f.updater.check({ force: true }); assert.equal(f.calls.length, 2); await preserved(f);
});

test('多个启动器同时恢复旧锁，只允许一个下载，活跃锁不被误删', async t => {
  const f = await fixture(t); await fs.mkdir(path.join(f.root, '.eve-updates'));
  const lock = path.join(f.root, '.eve-updates/update.lock');
  await fs.writeFile(lock, JSON.stringify({ pid: 99999999, protocol: 1 }));
  let release, requests = 0;
  const contenders = Array.from({ length: 8 }, () => createUpdater(f.root, { log: () => {}, fetchImpl: async () => {
    requests++; await new Promise(resolve => { release = resolve; }); throw new Error('synthetic offline');
  } }));
  const outcomes = contenders.map(updater => updater.check({ force: true }).catch(error => error));
  while (!release) await new Promise(resolve => setTimeout(resolve, 5));
  await new Promise(resolve => setTimeout(resolve, 50));
  assert.equal(requests, 1); assert.equal(JSON.parse(await fs.readFile(lock)).pid, process.pid);
  release(); await Promise.all(outcomes); assert.equal(requests, 1);
  await assert.rejects(fs.access(lock)); await preserved(f);
});

test('退出时中止正在传输的下载，释放锁并保留运行版本与用户数据', async t => {
  const f = await fixture(t), controller = new AbortController(); let transferring;
  const { files } = tree(), archive = archiveTar(files, 'Eve-v1.1.0-linux-x64');
  const updater = createUpdater(f.root, { log: () => {}, fetchImpl: async (url, options) => {
    if (url.includes('/releases/latest')) return new Response(JSON.stringify(releaseOf(archive)));
    return new Response(new ReadableStream({ start(stream) {
      transferring = true; stream.enqueue(archive.subarray(0, 20));
      options.signal.addEventListener('abort', () => stream.error(options.signal.reason), { once: true });
    } }));
  } });
  const result = updater.check({ force: true, signal: controller.signal }).catch(error => error);
  while (!transferring) await new Promise(resolve => setTimeout(resolve, 5));
  controller.abort(); assert(await result instanceof Error);
  assert.equal((await updater.status()).active, null); assert.equal((await updater.status()).pending, null);
  assert.deepEqual((await fs.readdir(path.join(f.root, '.eve-updates'))).sort(), ['versions']);
  await preserved(f);
});
