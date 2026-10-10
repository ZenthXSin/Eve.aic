// 更新实现：公开发行下载、暂存、完整性校验与原子版本指针；用户文件始终留在安装根目录。
import fs from 'node:fs/promises';
import path from 'node:path';
import { createHash, randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { REPOSITORY, PROTOCOL, MAX_ARCHIVE_BYTES, currentManifest, updatePlan, verifyManifest, safeRelative } from './update-contract.mjs';
import { unpack } from './update-archive.mjs';

const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const fresh = () => ({ format_version: 1, active: null, previous: null, pending: null, pending_failed: false, checked_at_ms: 0, outcome: 'idle' });
export class UpdateBusy extends Error {}

async function readJson(file) {
  try {
    const bytes = await fs.readFile(file);
    if (bytes.length > 2 * 1024 * 1024) throw new Error('更新状态容量无效。');
    return JSON.parse(bytes);
  } catch (error) { if (error.code === 'ENOENT') return null; throw new Error('更新状态无法读取；保留原文件。'); }
}
async function directory(file) {
  await fs.mkdir(file, { recursive: true });
  const info = await fs.lstat(file);
  if (!info.isDirectory() || info.isSymbolicLink()) throw new Error('更新目录不是普通目录。');
}
async function atomic(file, value) {
  const temporary = file + '.' + randomUUID() + '.tmp';
  const handle = await fs.open(temporary, 'wx', 0o600);
  try { await handle.writeFile(JSON.stringify(value, null, 2) + '\n'); await handle.sync(); }
  finally { await handle.close(); }
  try {
    for (let attempt = 0;; attempt++) {
      try { await fs.rename(temporary, file); break; }
      catch (error) {
        if (!['EPERM', 'EACCES', 'EBUSY'].includes(error.code) || attempt === 5) throw error;
        await new Promise(resolve => setTimeout(resolve, 100));
      }
    }
    if (process.platform !== 'win32') {
      const parent = await fs.open(path.dirname(file), 'r');
      try { await parent.sync(); } finally { await parent.close(); }
    }
  } finally { await fs.rm(temporary, { force: true }); }
}
function reference(value) {
  if (value === null) return null;
  if (!value || typeof value.slot !== 'string' || value.slot.includes('/') || !/^v\d+\.\d+\.\d+-[a-f0-9]{12}-[a-f0-9-]{36}$/.test(value.slot) ||
      !/^[a-f0-9]{64}$/.test(value.manifest_sha256 ?? '') || typeof value.version !== 'string') throw new Error('更新版本指针损坏；保留原文件。');
  safeRelative(value.slot); return value;
}
function state(value) {
  if (!value) return fresh();
  if (value.format_version !== 1 || typeof value.pending_failed !== 'boolean' || !Number.isSafeInteger(value.checked_at_ms) ||
      !['idle', 'current', 'downloaded', 'failed', 'activated', 'rollback', 'invalid_pending', 'invalid_active'].includes(value.outcome)) throw new Error('更新状态格式不支持；保留原文件。');
  for (const key of ['active', 'previous', 'pending']) reference(value[key]);
  return value;
}
async function runCheck(program, args = ['--help']) {
  const child = spawn(program, args, { stdio: 'ignore', windowsHide: true });
  const timer = setTimeout(() => child.kill(), 10000);
  try {
    await new Promise((resolve, reject) => {
      child.once('error', () => reject(new Error('新版运行器无法启动。')));
      child.once('exit', code => code === 0 ? resolve() : reject(new Error('新版运行器自检未通过。')));
    });
  } finally { clearTimeout(timer); }
}

export function createUpdater(root, { fetchImpl = globalThis.fetch, log = console.log, configPath, mode = 'qq', runProgram = runCheck } = {}) {
  root = path.resolve(root);
  const store = path.join(root, '.eve-updates'), versions = path.join(store, 'versions');
  const stateFile = path.join(store, 'installation.json'), lockFile = path.join(store, 'update.lock');
  const save = value => atomic(stateFile, value);
  async function setup() { await directory(store); await directory(versions); }
  async function withLock(callback) {
    await setup(); let lock;
    for (let attempt = 0; attempt < 2; attempt++) {
      try { lock = await fs.open(lockFile, 'wx', 0o600); break; }
      catch (error) {
        if (error.code !== 'EEXIST') throw error;
        const old = await readJson(lockFile);
        if (!Number.isSafeInteger(old?.pid) || old.pid < 1 || old.protocol !== PROTOCOL) throw new Error('更新锁损坏；保留原文件。');
        try { process.kill(old.pid, 0); throw new UpdateBusy('另一个启动器正在检查更新。'); }
        catch (alive) { if (alive.code !== 'ESRCH') throw alive; }
        // 同一旧锁只允许一个恢复者，避免并发恢复者误删新进程刚创建的锁。
        const recoveryFile = path.join(store, 'recovery-' + sha(Buffer.from(JSON.stringify(old))) + '.lock');
        let recovery;
        try { recovery = await fs.open(recoveryFile, 'wx', 0o600); }
        catch (error) { if (error.code === 'EEXIST') throw new UpdateBusy('另一个启动器正在恢复更新锁。'); throw error; }
        try {
          await recovery.writeFile(JSON.stringify({ pid: process.pid, protocol: PROTOCOL })); await recovery.sync();
          const latest = await readJson(lockFile);
          if (JSON.stringify(latest) !== JSON.stringify(old)) throw new UpdateBusy('更新锁已经由另一个启动器接管。');
          await fs.unlink(lockFile);
          try { lock = await fs.open(lockFile, 'wx', 0o600); }
          catch (error) { if (error.code === 'EEXIST') throw new UpdateBusy('另一个启动器正在检查更新。'); throw error; }
        } finally { await recovery.close(); await fs.unlink(recoveryFile); }
        if (lock) break;
      }
    }
    if (!lock) throw new UpdateBusy('另一个启动器正在检查更新。');
    try {
      await lock.writeFile(JSON.stringify({ pid: process.pid, protocol: PROTOCOL, nonce: randomUUID() })); await lock.sync();
      return await callback();
    } finally { await lock.close(); await fs.unlink(lockFile); }
  }
  async function metadata(appRoot = root) {
    return currentManifest(await readJson(path.join(appRoot, 'build-info.json')));
  }
  async function checkedSlot(ref, current, help = false, validateConfig = false) {
    reference(ref);
    const appRoot = path.join(versions, ref.slot);
    const info = await fs.lstat(appRoot);
    if (!info.isDirectory() || info.isSymbolicLink()) throw new Error('更新版本目录无效。');
    const real = await fs.realpath(appRoot), parent = await fs.realpath(versions);
    if (path.dirname(real) !== parent) throw new Error('更新版本目录越界。');
    const raw = await fs.readFile(path.join(appRoot, 'build-info.json'));
    if (sha(raw) !== ref.manifest_sha256) throw new Error('更新版本清单已改变。');
    const manifest = JSON.parse(raw);
    const plan = { version: ref.version, target: current.target, label: current.platform_label };
    const entries = verifyManifest(manifest, plan);
    for (const [name, expected] of entries) {
      let ancestor = appRoot;
      for (const component of name.split('/')) {
        ancestor = path.join(ancestor, component);
        const entry = await fs.lstat(ancestor);
        if (entry.isSymbolicLink()) throw new Error('更新版本不允许链接。');
      }
      const file = path.join(appRoot, name);
      if (!(await fs.lstat(file)).isFile() || sha(await fs.readFile(file)) !== expected) throw new Error('更新版本文件已损坏。');
    }
    if (help) {
      const extension = current.platform_label.startsWith('windows-') ? '.exe' : '';
      for (const binary of ['eve', 'eve-qqbot', 'eve-cognition', 'eve-memory', 'eve-message-evaluate']) await runProgram(path.join(appRoot, binary + extension));
      const node = path.join(appRoot, extension ? 'runtime/node.exe' : 'runtime/bin/node');
      await runProgram(node);
      await runProgram(node, ['--check', path.join(appRoot, 'Launch.mjs')]);
      if (validateConfig) await runProgram(node, [path.join(appRoot, 'Launch.mjs'), mode, '--validate-only', '--no-prompt', '--skip-update', '--user-root', root,
        ...(configPath ? ['--config-path', configPath] : [])]);
    }
    return { appRoot, manifest };
  }
  async function get(url, limit, signal, seconds) {
    const combined = AbortSignal.any([signal || new AbortController().signal, AbortSignal.timeout(seconds * 1000)]);
    for (let redirects = 0; redirects < 6; redirects++) {
      const parsed = new URL(url);
      if (parsed.protocol !== 'https:' || parsed.username || parsed.password || parsed.port ||
          !(['api.github.com', 'github.com'].includes(parsed.hostname) || parsed.hostname.endsWith('.githubusercontent.com'))) throw new Error('更新下载地址不受信任。');
      const response = await fetchImpl(url, { signal: combined, redirect: 'manual', headers: { 'User-Agent': 'EvePortableUpdater/1', Accept: 'application/vnd.github+json' } });
      if ([301, 302, 303, 307, 308].includes(response.status)) { await response.body?.cancel(); url = new URL(response.headers.get('location'), url).href; continue; }
      if (!response.ok || !response.body) throw new Error('更新服务暂不可用。');
      const chunks = []; let size = 0;
      const reader = response.body.getReader();
      try {
        for (;;) {
          const { value, done } = await reader.read(); if (done) break;
          size += value.length;
          if (size > limit) throw new Error('更新下载容量超限。');
          chunks.push(Buffer.from(value));
        }
      } finally { await reader.cancel().catch(() => {}); }
      return Buffer.concat(chunks, size);
    }
    throw new Error('更新下载重定向过多。');
  }
  async function status() { return state(await readJson(stateFile)); }
  async function resolveInstalled({ activate = true } = {}) {
    const current = await metadata();
    async function choose(write) {
      let saved = await status();
      if (write && activate && saved.pending && !saved.pending_failed) {
        let verified = false;
        try { await checkedSlot(saved.pending, current, true, true); verified = true; }
        catch {
          saved.pending_failed = true; saved.outcome = 'invalid_pending'; await save(saved);
          log('新版自检未通过，保留原版本；可重新检查更新。');
        }
        if (verified) {
          const next = { ...saved, previous: saved.active, active: saved.pending, pending: null, outcome: 'activated' };
          await save(next); saved = next; log(`已切换到 Eve ${saved.active.version}。`);
        }
      }
      if (saved.active) {
        try { return await checkedSlot(saved.active, current); }
        catch {
          log('当前更新版本校验未通过，尝试保留的上一版本。');
          if (saved.previous) {
            const previous = await checkedSlot(saved.previous, current);
            if (write) { const failed = saved.active; saved.active = saved.previous; saved.previous = failed; saved.outcome = 'invalid_active'; await save(saved); }
            return previous;
          }
          if (write) { saved.previous = saved.active; saved.active = null; saved.outcome = 'invalid_active'; await save(saved); }
        }
      }
      return { appRoot: root, manifest: current };
    }
    try { return await withLock(() => choose(true)); }
    catch (error) { if (error instanceof UpdateBusy) return choose(false); throw error; }
  }
  async function check({ signal, force = false, intervalMs = 6 * 3600000 } = {}) {
    return withLock(async () => {
      const current = await metadata(), saved = await status(); let staging, committing = false;
      const commit = async value => { committing = true; await save(value); committing = false; };
      if (!force && Date.now() - saved.checked_at_ms < intervalMs) return saved;
      try {
        const installed = saved.active ? (await checkedSlot(saved.active, current)).manifest : current;
        const release = JSON.parse((await get(`https://api.github.com/repos/${REPOSITORY}/releases/latest`, 2 * 1024 * 1024, signal, 20)).toString('utf8'));
        const plan = updatePlan(release, installed);
        saved.checked_at_ms = Date.now();
        if (!plan) { saved.outcome = 'current'; await commit(saved); return saved; }
        if (!force && saved.pending?.version === plan.version) return saved;
        const archive = await get(plan.url, Math.min(MAX_ARCHIVE_BYTES, plan.size), signal, 120);
        if (archive.length !== plan.size || sha(archive) !== plan.sha256) throw new Error('新版下载校验未通过。');
        const files = unpack(archive, plan), raw = files.get('build-info.json')?.bytes;
        if (!raw) throw new Error('新版缺少发行清单。');
        const manifest = JSON.parse(raw), entries = verifyManifest(manifest, plan, [...files.keys()]);
        for (const [name, expected] of entries) if (sha(files.get(name).bytes) !== expected) throw new Error('新版文件校验未通过。');
        staging = path.join(store, 'staging-' + randomUUID()); await fs.mkdir(staging);
        for (const [name, entry] of files) {
          if (signal?.aborted) throw new Error('更新下载已停止。');
          const file = path.join(staging, name); await fs.mkdir(path.dirname(file), { recursive: true });
          const handle = await fs.open(file, 'wx', entry.mode || 0o644);
          try { await handle.writeFile(entry.bytes); await handle.sync(); } finally { await handle.close(); }
        }
        const slot = `${plan.version}-${manifest.source_commit.slice(0, 12)}-${randomUUID()}`;
        await fs.rename(staging, path.join(versions, slot)); staging = null;
        const pending = { slot, version: plan.version, manifest_sha256: sha(raw) };
        await checkedSlot(pending, current, true);
        saved.pending = pending; saved.pending_failed = false; saved.outcome = 'downloaded'; await commit(saved);
        log(`Eve ${plan.version} 已下载并校验，下次启动自动启用。`); return saved;
      } catch (error) {
        if (committing) {
          log('更新状态提交无法确认；原文件保留，下次启动重新核对。');
        } else if (!signal?.aborted) {
          saved.checked_at_ms = Date.now(); saved.outcome = 'failed'; await save(saved);
          log('自动更新检查或下载失败，继续使用原版本；下次检查将重试。');
        }
        throw error;
      } finally { if (staging) await fs.rm(staging, { recursive: true, force: true }); }
    });
  }
  async function rollback() {
    return withLock(async () => {
      const current = await metadata(), saved = await status();
      if (saved.previous) await checkedSlot(saved.previous, current, true);
      if (!saved.active && !saved.pending) throw new Error('没有可以回退的更新版本。');
      const old = saved.active; saved.active = saved.previous; saved.previous = old;
      saved.pending = null; saved.pending_failed = false; saved.outcome = 'rollback';
      await save(saved); log('已选择上一版本，下次启动生效；用户配置与数据保留。'); return saved;
    });
  }
  return { check, resolveInstalled, rollback, status, metadata };
}
