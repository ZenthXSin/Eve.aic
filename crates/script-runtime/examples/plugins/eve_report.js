// 真实 Eve 工具协议示例；tools 声明 write_file/send_file/query_file_delivery。
// 普通成员文件始终经过自己的项目工作区，声明本身不授予工具权限。
const readline = require('node:readline');
const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
const iterator = lines[Symbol.asyncIterator]();
async function receive() {
  const line = await iterator.next();
  if (line.done) throw new Error('host disconnected');
  return JSON.parse(line.value);
}
async function tool(name, arguments_) {
  process.stdout.write(JSON.stringify({ op: 'tool', name, arguments: arguments_ }) + '\n');
  const response = await receive();
  if (!response.ok) throw new Error('host rejected');
  return response.result;
}
(async () => {
  const call = await receive();
  const name = `report-${call.call_id}.txt`;
  const path = `exports/${name}`;
  // 空 expected_sha256 只允许创建新文件，永不覆盖既有文件。
  await tool('write_file', { path, content: `报告: ${call.arguments.title}\n`, expected_sha256: '' });
  const queued = await tool('send_file', { path, name });
  const delivery = await tool('query_file_delivery', { id: queued.id });
  // Pending/Sending/Unknown 均不当作送达；不重发。
  const delivered = delivery.sent === true && !!delivery.message_id && !!delivery.session_id;
  process.stdout.write(JSON.stringify({ op: 'return', value: { queued, delivery, delivered } }) + '\n');
  lines.close();
})().catch(() => { process.exitCode = 1; lines.close(); });
