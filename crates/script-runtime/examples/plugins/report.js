// JavaScript 子插件：只通过 JSON RPC 请求共享文件和已授权交付工具。
const readline = require('node:readline');
const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
const iterator = lines[Symbol.asyncIterator]();
async function receive() {
  const line = await iterator.next();
  if (line.done) throw new Error('host disconnected');
  return JSON.parse(line.value);
}
async function rpc(request) {
  process.stdout.write(JSON.stringify(request) + '\n');
  const response = await receive();
  if (!response.ok) throw new Error('host rejected');
  return response.result;
}
(async () => {
  const invocation = await receive();
  const text = `本地模拟报告: ${invocation.arguments.title}\n`;
  await rpc({ op: 'write', path: 'report.txt', content: text });
  // $workspace 由宿主转换为临时文件路径；脚本从不取得宿主凭据。
  const delivered = await rpc({ op: 'tool', name: 'qq.send_file', arguments: {
    path: { $workspace: 'report.txt' }, name: 'report.txt'
  }});
  await rpc({ op: 'artifact', path: 'report.txt' });
  await rpc({ op: 'state', value: { count: invocation.state.value.count + 1 } });
  process.stdout.write(JSON.stringify({ op: 'return', value: { delivered, text } }) + '\n');
  lines.close();
})().catch(() => { process.exitCode = 1; lines.close(); });
