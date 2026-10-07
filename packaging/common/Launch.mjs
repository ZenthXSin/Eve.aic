import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';

const root = fileURLToPath(new URL('.', import.meta.url));
const windows = process.platform === 'win32';
const nodePath = path.join(root, windows ? 'runtime/node.exe' : 'runtime/bin/node');
const argv = process.argv.slice(2);
const mode = argv.shift() || 'qq';
const options = { noPrompt: false, validateOnly: false, bridge: '', bridgeArg: '' };
for (let i = 0; i < argv.length; i++) {
  const flag = argv[i];
  if (flag === '--no-prompt') options.noPrompt = true;
  else if (flag === '--validate-only') options.validateOnly = true;
  else if (flag === '--bridge-script' && argv[i + 1]) options.bridge = argv[++i];
  else if (flag === '--bridge-arg' && argv[i + 1]) options.bridgeArg = argv[++i];
  else throw new Error('启动参数无效。');
}

async function readSecret(label) {
  if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error('首次配置需要交互终端，或先填写 config.json。');
  process.stdout.write(label + '（输入不显示）：');
  const previousRaw = process.stdin.isRaw;
  process.stdin.setRawMode(true);
  process.stdin.resume();
  return await new Promise((resolve, reject) => {
    let value = '';
    const finish = (error) => {
      process.stdin.off('data', onData);
      process.stdin.setRawMode(previousRaw);
      process.stdin.pause();
      process.stdout.write('\n');
      if (error) reject(error); else resolve(value);
    };
    const onData = buffer => {
      for (const character of buffer.toString('utf8')) {
        if (character === '\u0003' || character === '\u0004') { finish(new Error('配置已取消，原文件未修改。')); return; }
        if (character === '\r' || character === '\n') { finish(); return; }
        if (character === '\u007f' || character === '\b') value = value.slice(0, -1);
        else if (character >= ' ') value += character;
      }
    };
    process.stdin.on('data', onData);
  });
}

async function loadConfig() {
  const configPath = path.join(root, 'config.json');
  let contents;
  try { contents = await fs.readFile(configPath, 'utf8'); }
  catch (error) {
    if (error.code !== 'ENOENT') throw new Error('config.json 无法读取，原文件未修改。');
    if (options.noPrompt || options.validateOnly || mode === 'panel') throw new Error('请先运行 Start-Eve 完成配置。');
    contents = await fs.readFile(path.join(root, 'config.example.json'), 'utf8');
  }
  try { return { config: JSON.parse(contents), configPath }; }
  catch { throw new Error('config.json 不是有效 JSON，原文件未修改。'); }
}

function validText(value) { return typeof value === 'string' && value.trim().length > 0; }
function validate(config) {
  if (config.format_version !== 1) throw new Error('配置版本不支持。');
  if (!validText(config.model?.api_key) || !validText(config.model?.name)) throw new Error('模型名称或 API 密钥为空。');
  if (!['chat', 'responses'].includes(config.model.protocol)) throw new Error('模型协议只能为 chat 或 responses。');
  for (const key of ['training', 'cognition', 'self_learning', 'memory_recall', 'segmented']) {
    if (typeof config.features?.[key] !== 'boolean') throw new Error(`features.${key} 必须为布尔值。`);
  }
  if (mode === 'console') return;
  if (!/^[0-9]{1,32}$/.test(config.qq?.app_id) || !validText(config.qq?.app_secret)) throw new Error('QQ AppID 或 Secret 无效。');
  if (typeof config.qq.sandbox !== 'boolean' || typeof config.jev?.enabled !== 'boolean') throw new Error('QQ / Jev 开关必须为布尔值。');
  if (!/^[\x21-\x7e]{32,256}$/.test(config.web?.token)) throw new Error('面板令牌无效。');
  const match = /^127\.0\.0\.1:([0-9]{1,5})$/.exec(config.web.listen);
  if (!match || Number(match[1]) > 65535) throw new Error('面板地址必须为本机 127.0.0.1:端口。');
  if (config.jev.enabled && !validText(config.jev.api_key)) throw new Error('Jev 独立密钥为空。');
}

async function main() {
  if (!['qq', 'console', 'panel'].includes(mode)) throw new Error('仅支持 qq / console / panel 模式。');
  const { config, configPath } = await loadConfig();
  let changed = false;
  if (!options.noPrompt && !options.validateOnly && mode !== 'panel') {
    if (!validText(config.model.api_key)) { config.model.api_key = await readSecret('请输入主模型 API 密钥'); changed = true; }
    if (mode === 'qq') {
      if (!validText(config.qq.app_secret)) { config.qq.app_secret = await readSecret('请输入 QQ App Secret'); changed = true; }
      if (config.jev.enabled && !validText(config.jev.api_key)) { config.jev.api_key = await readSecret('请输入 OpenRouter / Jev 密钥'); changed = true; }
      if (!validText(config.web.token)) { config.web.token = randomBytes(32).toString('hex'); changed = true; }
    }
  }
  validate(config);
  if (mode === 'panel') {
    if (config.web.listen.endsWith(':0')) throw new Error('端口 0 请使用 EVE_WEB_READY 输出的实际地址。');
    console.log(`地址：http://${config.web.listen}\n本机登录令牌：${config.web.token}`);
    const url = `http://${config.web.listen}`;
    const browser = windows ? 'rundll32.exe' : process.platform === 'darwin' ? 'open' : 'xdg-open';
    const open = spawn(browser, windows ? ['url.dll,FileProtocolHandler', url] : [url], { detached: true, stdio: 'ignore' });
    open.on('error', () => console.log('请手动在浏览器打开上述地址。'));
    open.unref();
    return;
  }
  const binary = path.join(root, (mode === 'qq' ? 'eve-qqbot' : 'eve') + (windows ? '.exe' : ''));
  const bridge = options.bridge || path.join(root, 'connectors/qqbot/bridge.mjs');
  const required = [binary, path.join(root, 'AGENT.md')];
  if (mode === 'qq') required.push(nodePath, bridge, path.join(root, 'connectors/qqbot/node_modules/@tencent-connect/qqbot-nodejs/package.json'));
  for (const file of required) await fs.access(file);
  if (options.validateOnly) { console.log('配置及发行包检查通过。'); return; }
  if (changed) {
    const temporary = configPath + `.tmp-${process.pid}`;
    try {
      await fs.writeFile(temporary, JSON.stringify(config, null, 2) + '\n', { flag: 'wx', mode: 0o600 });
      await fs.rename(temporary, configPath);
    } finally { await fs.rm(temporary, { force: true }); }
    console.log('配置已保存到本机 config.json，请保持此文件私有。');
  }
  const env = { ...process.env,
    EVE_OPENAI_API_KEY: config.model.api_key, EVE_OPENAI_BASE_URL: config.model.base_url,
    EVE_OPENAI_MODEL: config.model.name, EVE_OPENAI_PROTOCOL: config.model.protocol,
    EVE_OPENAI_MODEL_ROLE: '', EVE_OPENAI_REASONING_EFFORT: 'none', EVE_LLM_RESPONSE_MODE: 'complete' };
  const args = ['--state-dir', path.join(root, 'data', mode), '--agent', path.join(root, 'AGENT.md')];
  if (config.features.segmented) args.push('--segmented');
  if (mode === 'qq') {
    Object.assign(env, { QQBOT_APP_ID: config.qq.app_id, QQBOT_APP_SECRET: config.qq.app_secret,
      QQBOT_SANDBOX: String(config.qq.sandbox), EVE_WEB_TOKEN: config.web.token, EVE_JEV_API_KEY: '' });
    args.push('--node', nodePath, '--bridge-script', bridge, '--memory', '--web-listen', config.web.listen);
    if (options.bridgeArg) args.push('--bridge-arg', options.bridgeArg);
    for (const [key, flag] of [['training', '--training'], ['cognition', '--cognition'], ['self_learning', '--self-learning'], ['memory_recall', '--memory-recall']]) {
      if (config.features[key]) args.push(flag);
    }
    if (config.jev.enabled) {
      Object.assign(env, { EVE_JEV_API_KEY: config.jev.api_key, EVE_JEV_BASE_URL: config.jev.base_url,
        EVE_MODELS_JEV_ENABLED: 'true', EVE_MODELS_JEV_PROVIDER: 'jev', EVE_MODELS_JEV_MODEL: config.jev.model,
        EVE_MODELS_JEV_CREDENTIAL_REF: 'env:EVE_JEV_API_KEY', EVE_MODELS_JEV_TIMEOUT_MS: '3000',
        EVE_MODELS_JEV_MAX_CONCURRENT_REQUESTS: '4', EVE_MODELS_JEV_MAX_OUTPUT_TOKENS: '0', EVE_MESSAGE_JUDGE_TIMEOUT_MS: '6000' });
      args.push('--message-judge', 'jev');
    }
    console.log('QQ 启动中；看到 EVE_QQBOT_READY / EVE_WEB_READY 后打开 Open-Panel。');
  }
  const child = spawn(binary, args, { cwd: root, env, stdio: 'inherit' });
  // Windows 控制台向父子进程同时广播 Ctrl+C；Node 的 kill(SIGINT) 会强制终止，
  // 因此这里只保留父进程等待，让 Rust 自己完成控制台信号收尾。
  const interrupt = () => { if (!windows) child.kill('SIGINT'); };
  const terminate = () => child.kill('SIGTERM');
  process.on('SIGINT', interrupt);
  process.on('SIGTERM', terminate);
  try {
    process.exitCode = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('exit', (code) => resolve(code ?? 1));
    });
  } finally {
    process.off('SIGINT', interrupt);
    process.off('SIGTERM', terminate);
  }
}

main().catch(error => { console.error('Eve 启动失败：' + error.message); process.exitCode = 1; });
