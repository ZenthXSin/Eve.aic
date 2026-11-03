// 真实启动器进程与本地参数捕获替身：只核对装配参数，不连接 QQ 或模型。
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
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

async function webFixture(t) {
  const f = await fixture(t);
  delete f.config.qq;
  delete f.config.jev;
  await fs.writeFile(path.join(f.root, 'eve-web'), `#!${process.execPath}
const fs = require('node:fs');
const config = JSON.parse(fs.readFileSync('config.json', 'utf8'));
fs.writeFileSync('web-captured.json', JSON.stringify({
  args: process.argv.slice(2),
  token_matches_config: process.env.EVE_WEB_TOKEN === config.web.token,
  token_is_random_32_bytes: /^[0-9a-f]{64}$/.test(process.env.EVE_WEB_TOKEN),
  model_matches_config: process.env.EVE_OPENAI_API_KEY === config.model.api_key,
}));
`, { mode: 0o700 });
  return f;
}

function launchWeb(f, extra = []) {
  return spawnSync(process.execPath, [path.join(f.root, 'Launch.mjs'), 'web', '--skip-update', ...extra],
    { encoding: 'utf8', timeout: 10000 });
}

// script 为真实 Node 子进程分配 PTY，覆盖 readSecret 的原始终端输入而不修改启动器。
async function firstWebLaunch(t, f, modelKey) {
  const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
  const command = [process.execPath, path.join(f.root, 'Launch.mjs'), 'web', '--skip-update'].map(quote).join(' ');
  const child = spawn('script', ['--quiet', '--return', '--command', command, '/dev/null'], { stdio: ['pipe', 'pipe', 'pipe'] });
  t.after(() => { if (child.exitCode === null) child.kill('SIGKILL'); });
  let output = '', entered = false;
  const timer = setTimeout(() => child.kill('SIGKILL'), 10000);
  const read = data => {
    output += data.toString('utf8');
    if (!entered && output.includes('请输入主模型 API 密钥（输入不显示）：')) {
      entered = true;
      child.stdin.write(modelKey + '\n');
    }
  };
  child.stdout.on('data', read);
  child.stderr.on('data', read);
  try {
    const status = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', resolve);
    });
    return { status, output, entered };
  } finally { clearTimeout(timer); child.stdin.destroy(); }
}

for (const [name, value, enabled] of [['缺省开启', undefined, true], ['显式开启', true, true], ['显式关闭', false, false]]) {
  test(`启动器自然对话能力${name}并指定内置Node`, { skip: process.platform === 'win32' }, async t => {
    const f = await fixture(t), result = await f.launch(value);
    assert.equal(result.status, 0, result.stderr);
    const captured = JSON.parse(await fs.readFile(path.join(f.root, 'captured.json'), 'utf8'));
    for (const flag of capabilityFlags) assert.equal(captured.args.includes(flag), enabled, flag);
    assert.equal(captured.args.includes('--minimal-profile'), !enabled);
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

test('Web 模式没有 QQ 配置也能启动专用入口并传递面板令牌', { skip: process.platform === 'win32' }, async t => {
  const f = await fixture(t);
  delete f.config.qq;
  delete f.config.jev;
  f.config.features.segmented = true;
  await fs.writeFile(path.join(f.root,'config.json'),JSON.stringify(f.config));
  await fs.writeFile(path.join(f.root,'eve-web'),`#!${process.execPath}\nrequire('node:fs').writeFileSync('web-captured.json',JSON.stringify({args:process.argv.slice(2),token_configured:!!process.env.EVE_WEB_TOKEN}));\n`,{mode:0o700});
  const result=spawnSync(process.execPath,[path.join(f.root,'Launch.mjs'),'web','--no-prompt','--skip-update'],{encoding:'utf8',timeout:10000});
  assert.equal(result.status,0,result.stderr);
  const saved=JSON.parse(await fs.readFile(path.join(f.root,'web-captured.json'),'utf8'));
  assert.ok(saved.args.includes('--listen'));
  assert.ok(saved.args.includes('127.0.0.1:0'));
  assert.equal(saved.args.includes('--segmented'),false);
  assert.equal(saved.args.includes('--bridge-script'),false);
  assert.equal(saved.token_configured,true);
});

test('首次 Web 只输入模型凭据，随机令牌私密保存且重启后保持不变', { skip: process.platform !== 'linux' }, async t => {
  const f = await webFixture(t);
  f.config.model.api_key = '';
  f.config.web.token = '';
  const template = JSON.stringify(f.config);
  await fs.writeFile(path.join(f.root, 'config.example.json'), template);
  const modelKey = 'first-web-synthetic-model-key';
  const result = await firstWebLaunch(t, f, modelKey);
  assert.equal(result.status, 0, result.output);
  assert.equal(result.entered, true);
  assert.doesNotMatch(result.output, /请输入 QQ|请输入 OpenRouter|请输入.*令牌/);
  assert.equal(result.output.includes(modelKey), false, '输入凭据不应回显');
  const configPath = path.join(f.root, 'config.json');
  const savedBytes = await fs.readFile(configPath, 'utf8');
  const saved = JSON.parse(savedBytes);
  assert.equal(saved.model.api_key, modelKey);
  assert.match(saved.web.token, /^[0-9a-f]{64}$/);
  assert.equal(result.output.includes(saved.web.token), false, '启动日志不得显示令牌');
  assert.equal((await fs.stat(configPath)).mode & 0o777, 0o600);
  const captured = JSON.parse(await fs.readFile(path.join(f.root, 'web-captured.json'), 'utf8'));
  assert.equal(captured.token_matches_config, true);
  assert.equal(captured.token_is_random_32_bytes, true);
  assert.equal(captured.model_matches_config, true);
  assert.equal(await fs.readFile(path.join(f.root, 'config.example.json'), 'utf8'), template);
  assert.equal((await fs.readdir(f.root)).some(name => name.startsWith('config.json.tmp-')), false);

  const restart = launchWeb(f, ['--no-prompt']);
  assert.equal(restart.status, 0, restart.stderr);
  assert.equal(await fs.readFile(configPath, 'utf8'), savedBytes);
  assert.equal((restart.stdout + restart.stderr).includes(saved.web.token), false);
  assert.equal((restart.stdout + restart.stderr).includes(modelKey), false);
});

test('Web 已有模型配置时可无人值守生成缺省令牌，独立初始化不复用令牌', { skip: process.platform === 'win32' }, async t => {
  const tokens = [];
  for (const missing of [true, false]) {
    const f = await webFixture(t);
    if (missing) delete f.config.web.token;
    else f.config.web.token = '   ';
    await fs.writeFile(path.join(f.root, 'config.json'), JSON.stringify(f.config), { mode: 0o644 });
    const result = launchWeb(f, missing ? ['--no-prompt'] : []);
    assert.equal(result.status, 0, result.stderr);
    const saved = JSON.parse(await fs.readFile(path.join(f.root, 'config.json'), 'utf8'));
    assert.match(saved.web.token, /^[0-9a-f]{64}$/);
    assert.equal((await fs.stat(path.join(f.root, 'config.json'))).mode & 0o777, 0o600);
    assert.equal((result.stdout + result.stderr).includes(saved.web.token), false);
    tokens.push(saved.web.token);
  }
  assert.notEqual(tokens[0], tokens[1]);
});

test('Web 缺省令牌在只验证和打开面板时不改动状态，无人值守缺配置不创建文件', { skip: process.platform === 'win32' }, async t => {
  const f = await webFixture(t);
  f.config.web.token = '';
  const original = JSON.stringify(f.config);
  const configPath = path.join(f.root, 'config.json');
  await fs.writeFile(configPath, original);
  for (const args of [['web', '--validate-only'], ['panel']]) {
    const result = spawnSync(process.execPath, [path.join(f.root, 'Launch.mjs'), ...args, '--skip-update'],
      { encoding: 'utf8', timeout: 10000 });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /面板令牌无效/);
    assert.equal(await fs.readFile(configPath, 'utf8'), original);
    await assert.rejects(fs.stat(path.join(f.root, 'web-captured.json')), { code: 'ENOENT' });
  }
  await fs.rename(configPath, path.join(f.root, 'config.example.json'));
  for (const args of [['web', '--validate-only'], ['web', '--no-prompt'], ['panel']]) {
    const result = spawnSync(process.execPath, [path.join(f.root, 'Launch.mjs'), ...args, '--skip-update'],
      { encoding: 'utf8', timeout: 10000 });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /请先运行 Start-Eve 或 Start-Web 完成配置/);
    await assert.rejects(fs.stat(configPath), { code: 'ENOENT' });
  }
});

test('Web 非空无效令牌及其他配置错误不会被初始化覆盖', { skip: process.platform === 'win32' }, async t => {
  const f = await webFixture(t);
  for (const invalid of [{ token: 'too-short' }, { token: 1234 }, { token: null }, { token: '', listen: '0.0.0.0:8765' }]) {
    const config = structuredClone(f.config);
    Object.assign(config.web, invalid);
    const original = JSON.stringify(config);
    await fs.writeFile(path.join(f.root, 'config.json'), original);
    const result = launchWeb(f);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /面板令牌无效|面板地址必须为本机/);
    assert.equal(await fs.readFile(path.join(f.root, 'config.json'), 'utf8'), original);
    await assert.rejects(fs.stat(path.join(f.root, 'web-captured.json')), { code: 'ENOENT' });
  }
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
