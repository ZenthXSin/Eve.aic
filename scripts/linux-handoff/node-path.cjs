// 私有 ELF 加载器改变了内核看到的可执行路径；fork 必须重新经过 Node 包装器。
const path = require('node:path');
process.execPath = path.join(__dirname, 'bin', 'node');
process.argv[0] = process.execPath;
