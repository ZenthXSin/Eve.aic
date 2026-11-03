// 运行于已有 bubblewrap/容器沙箱；stdout 仅承载宿主 RPC。
const fs = require('node:fs');
let unread = Buffer.alloc(0);
function line() {
  const chunks = [];
  let size = 0;
  for (;;) {
    const end = unread.indexOf(10);
    const part = end < 0 ? unread : unread.subarray(0, end);
    size += part.length;
    if (size > 1048576) throw Error('host_response_too_large');
    chunks.push(part);
    if (end >= 0) {
      unread = unread.subarray(end + 1);
      return JSON.parse(Buffer.concat(chunks, size).toString('utf8'));
    }
    const buffer = Buffer.allocUnsafe(65536);
    const read = fs.readSync(0, buffer, 0, buffer.length, null);
    if (!read) throw Error('host_closed');
    unread = buffer.subarray(0, read);
  }
}
function send(value) { fs.writeSync(1, JSON.stringify(value) + '\n'); }
async function main(code, names) {
  try {
    const input = line();
    const pending = [];
    let scheduled = false;
    let outstanding = 0;
    const output = [];
    let bytes = 2;
    function flush() {
      scheduled = false;
      const batch = pending.splice(0, 8);
      send({op:'tools', calls:batch.map(item => ({name:item.name, arguments:item.arguments}))});
      const response = line();
      if (response.ok !== true) throw Error('host_rejected');
      const result = response.result;
      if (!Array.isArray(result.values) || result.values.length !== batch.length) throw Error('invalid_batch_response');
      batch.forEach((item, index) => { outstanding--; item.resolve(result.values[index]); });
      if (pending.length) { scheduled = true; queueMicrotask(flush); }
    }
    const tools = Object.create(null);
    for (const name of names) {
      tools[name] = arguments_ => new Promise(resolve => {
        if (outstanding >= 128) throw Error('too_many_pending_tools');
        outstanding++;
        pending.push({name, arguments:arguments_ ?? {}, resolve});
        if (!scheduled) { scheduled = true; queueMicrotask(flush); }
      });
    }
    Object.freeze(tools);
    function text(value) {
      const encoded = JSON.stringify(value ?? null);
      bytes += Buffer.byteLength(encoded, 'utf8') + 1;
      if (bytes > 32768 || output.length >= 64) throw Error('selected_output_too_large');
      output.push(JSON.parse(encoded));
    }
    const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
    await new AsyncFunction('tools', 'text', 'args', '"use strict";\n' + code)(tools, text, input.arguments);
    if (outstanding) throw Error('tool_promises_must_be_awaited');
    send({op:'return', value:{output}});
  } catch (_) { process.exitCode = 1; }
}
