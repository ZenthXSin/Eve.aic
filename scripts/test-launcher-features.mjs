// 真实启动器进程与本地参数捕获替身：只核对装配参数，不连接 QQ 或模型。
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const capabilityFlags = ['--background-tasks', '--subagents', '--triggers', '--sleep', '--workflow-tools', '--health-watch'];
async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'eve-launcher-features-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  for (const name of ['Launch.mjs', 'Updater.mjs', 'update-contract.mjs', 'update-archive.mjs']) {
    await fs.copyFile(path.join(repository, 'packaging/common', name), path.join(root, name));
  }
  await fs.mkdir(path.join(root, 'runtime/bin'), { recursive: true });
  await fs.writeFile(path.join(root, 'AGENT.md'), '# 合成启动器验收\n');
  await fs.mkdir(path.join(root, 'connectors/qqbot'), { recursive: true });
  await fs.writeFile(path.join(root, 'connectors/qqbot/bridge.mjs'), '// 参数替身不执行此文件\n');
  await fs.mkdir(path.join(root, 'connectors/qqbot/node_modules/@tencent-connect/qqbot-nodejs'), { recursive: true });
  await fs.writeFile(path.join(root, 'connectors/qqbot/node_modules/@tencent-connect/qqbot-nodejs/package.json'), '{}');
  await fs.symlink(process.execPath, path.join(root, 'runtime/bin/node'));
  await fs.writeFile(path.join(root, 'eve-qqbot'), `#!${process.execPath}
require('node:fs').writeFileSync('captured.json', JSON.stringify({args:process.argv.slice(2), script_node:process.env.EVE_SCRIPT_NODE}));
`, { mode: 0o700 });
  const config = {
    format_version: 1, model: { name: 'local-stub', protocol: 'chat', api_key: 'synthetic-model' },
    qq: { app_id: '1234', app_secret: 'synthetic-qq', sandbox: false }, jev: { enabled: false },
    web: { listen: '127.0.0.1:0', token: 'synthetic-panel-token-12345678901234567890' },
    updates: { enabled: false }, features: { training: false, cognition: false, self_learning: false, memory_recall: false, segmented: false },
  };
  return { root, config, async launch(value, extra = []) {
    const next = structuredClone(config);
    if (value !== undefined) next.features.autonomous_tools = value;
    await fs.writeFile(path.join(root, 'config.json'), JSON.stringify(next));
    return spawnSync(process.execPath, [path.join(root, 'Launch.mjs'), 'qq', '--no-prompt', '--skip-update', ...extra], { encoding: 'utf8', timeout: 10000 });
  } };
}

for (const [name, value, enabled] of [['缺省开启', undefined, true], ['显式开启', true, true], ['显式关闭', false, false]]) {
  test(`启动器自然对话能力${name}并指定内置Node`, { skip: process.platform === 'win32' }, async t => {
    const f = await fixture(t), result = await f.launch(value);
    assert.equal(result.status, 0, result.stderr);
    const captured = JSON.parse(await fs.readFile(path.join(f.root, 'captured.json'), 'utf8'));
    for (const flag of capabilityFlags) assert.equal(captured.args.includes(flag), enabled, flag);
    assert.equal(captured.script_node, path.join(f.root, 'runtime/bin/node'));
    assert.equal(captured.args.includes('--memory'), true);
  });
}
test('启动器拒绝非布尔能力开关，宿主尚未启动', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t);
  for (const value of ['false', 0, null, {}]) {
    const result = await f.launch(value);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /features\.autonomous_tools 必须为布尔值/);
    await assert.rejects(fs.stat(path.join(f.root, 'captured.json')), { code: 'ENOENT' });
  }
});
test('仅验证配置时不启动宿主或默认能力', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t), result = await f.launch(true, ['--validate-only']);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /配置及发行包检查通过/);
  await assert.rejects(fs.stat(path.join(f.root, 'captured.json')), { code: 'ENOENT' });
});
test('不支持的平台在读取用户配置或启动宿主前明确拒绝', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t);
  const original = JSON.stringify(f.config);
  await fs.writeFile(path.join(f.root, 'config.json'), original);
  for (const platform of ['darwin', 'freebsd']) {
    const result = spawnSync(process.execPath, ['--input-type=module', '-e',
      `Object.defineProperty(process, 'platform', { value: ${JSON.stringify(platform)} }); await import(${JSON.stringify(pathToFileURL(path.join(f.root, 'Launch.mjs')).href)});`],
    { encoding: 'utf8', timeout: 10000 });
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, /仅支持 Linux 和 Windows/);
    assert.equal(await fs.readFile(path.join(f.root, 'config.json'), 'utf8'), original);
    await assert.rejects(fs.stat(path.join(f.root, 'captured.json')), { code: 'ENOENT' });
  }
});
