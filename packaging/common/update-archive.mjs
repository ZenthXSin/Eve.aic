// 受限 ZIP/tar.gz 解码：仅目录与常规文件；校验完成后由安装器写入私有暂存目录。
import { crc32, gunzipSync, inflateRawSync } from 'node:zlib';
import { MAX_UNPACKED_BYTES, MAX_FILES, safeRelative } from './update-contract.mjs';

function add(files, name, bytes, mode, folder, totals) {
  safeRelative(name);
  if (!name.startsWith(folder + '/')) throw new Error('更新包目录不符。');
  const relative = safeRelative(name.slice(folder.length + 1));
  if (files.has(relative) || files.size >= MAX_FILES || (totals.bytes += bytes.length) > MAX_UNPACKED_BYTES) throw new Error('更新包重复文件或容量超限。');
  files.set(relative, { bytes, mode: mode & 0o777 });
}
function zip(buffer, folder) {
  let end = -1;
  for (let at = buffer.length - 22; at >= Math.max(0, buffer.length - 65557); at--) {
    if (buffer.readUInt32LE(at) === 0x06054b50 && at + 22 + buffer.readUInt16LE(at + 20) === buffer.length) { end = at; break; }
  }
  if (end < 0 || buffer.readUInt16LE(end + 4) || buffer.readUInt16LE(end + 6)) throw new Error('更新 ZIP 格式不支持。');
  const count = buffer.readUInt16LE(end + 10), centralSize = buffer.readUInt32LE(end + 12);
  let offset = buffer.readUInt32LE(end + 16);
  if (count > MAX_FILES * 2 || offset + centralSize !== end || buffer.readUInt16LE(end + 8) !== count) throw new Error('更新 ZIP 清单无效。');
  const files = new Map(), totals = { bytes: 0 };
  for (let i = 0; i < count; i++) {
    if (offset + 46 > end || buffer.readUInt32LE(offset) !== 0x02014b50) throw new Error('更新 ZIP 条目损坏。');
    const flags = buffer.readUInt16LE(offset + 8), method = buffer.readUInt16LE(offset + 10);
    const packed = buffer.readUInt32LE(offset + 20), unpacked = buffer.readUInt32LE(offset + 24);
    const nameLength = buffer.readUInt16LE(offset + 28), extraLength = buffer.readUInt16LE(offset + 30), commentLength = buffer.readUInt16LE(offset + 32);
    const mode = buffer.readUInt32LE(offset + 38) >>> 16, local = buffer.readUInt32LE(offset + 42);
    if (flags & 1 || ![0, 8].includes(method) || offset + 46 + nameLength + extraLength + commentLength > end ||
        local + 30 > buffer.length || buffer.readUInt32LE(local) !== 0x04034b50 || unpacked > MAX_UNPACKED_BYTES - totals.bytes) throw new Error('更新 ZIP 条目不支持或容量超限。');
    const nameBytes = buffer.subarray(offset + 46, offset + 46 + nameLength), name = nameBytes.toString('utf8');
    if (!Buffer.from(name).equals(nameBytes)) throw new Error('更新文件名编码无效。');
    const localNameLength = buffer.readUInt16LE(local + 26), localExtraLength = buffer.readUInt16LE(local + 28);
    if (!buffer.subarray(local + 30, local + 30 + localNameLength).equals(nameBytes) || buffer.readUInt16LE(local + 8) !== method) throw new Error('更新 ZIP 本地条目不符。');
    const start = local + 30 + localNameLength + localExtraLength;
    if (start + packed > buffer.readUInt32LE(end + 16)) throw new Error('更新 ZIP 数据越界。');
    const type = mode & 0o170000;
    if (type && ![0o100000, 0o040000].includes(type)) throw new Error('更新包不允许链接或特殊文件。');
    if (name.endsWith('/')) {
      const directory = name.slice(0, -1); safeRelative(directory);
      if (directory !== folder && !directory.startsWith(folder + '/')) throw new Error('更新包目录不符。');
    } else {
      const data = buffer.subarray(start, start + packed);
      const bytes = method === 8 ? inflateRawSync(data, { maxOutputLength: Math.max(1, unpacked) }) : Buffer.from(data);
      if (bytes.length !== unpacked || crc32(bytes) !== buffer.readUInt32LE(offset + 16)) throw new Error('更新 ZIP 文件校验不符。');
      add(files, name, bytes, mode || 0o644, folder, totals);
    }
    offset += 46 + nameLength + extraLength + commentLength;
  }
  if (offset !== end) throw new Error('更新 ZIP 清单长度不符。');
  return files;
}
function octal(bytes) {
  const text = bytes.toString('ascii').replace(/\0/g, '').trim();
  if (!/^[0-7]*$/.test(text)) throw new Error('更新 tar 数值无效。');
  return parseInt(text || '0', 8);
}
function cstring(bytes) { return bytes.subarray(0, bytes.indexOf(0) < 0 ? bytes.length : bytes.indexOf(0)).toString('utf8'); }
function pax(bytes) {
  const result = {};
  for (let at = 0; at < bytes.length;) {
    const space = bytes.indexOf(32, at), length = Number(bytes.subarray(at, space).toString('ascii'));
    if (space < at || !Number.isSafeInteger(length) || length < space - at + 3 || at + length > bytes.length || bytes[at + length - 1] !== 10) throw new Error('更新 tar 扩展记录无效。');
    const record = bytes.subarray(space + 1, at + length - 1).toString('utf8'), equal = record.indexOf('=');
    if (equal <= 0) throw new Error('更新 tar 扩展记录无效。');
    result[record.slice(0, equal)] = record.slice(equal + 1); at += length;
  }
  if (result.linkpath || Object.keys(result).some(key => key.startsWith('GNU.sparse'))) throw new Error('更新包不允许链接或稀疏文件。');
  return result;
}
function tar(archive, folder) {
  const buffer = gunzipSync(archive, { maxOutputLength: MAX_UNPACKED_BYTES + MAX_FILES * 4096 });
  const files = new Map(), totals = { bytes: 0 }; let extended = {}, ended = false;
  for (let offset = 0; offset + 512 <= buffer.length;) {
    const header = buffer.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) { ended = true; break; }
    const expected = octal(header.subarray(148, 156)); let sum = 0;
    for (let i = 0; i < 512; i++) sum += i >= 148 && i < 156 ? 32 : header[i];
    if (sum !== expected) throw new Error('更新 tar 头校验不符。');
    const size = octal(header.subarray(124, 136)), mode = octal(header.subarray(100, 108)), type = String.fromCharCode(header[156]);
    if (offset + 512 + size > buffer.length) throw new Error('更新 tar 文件越界。');
    const bytes = buffer.subarray(offset + 512, offset + 512 + size);
    offset += 512 + Math.ceil(size / 512) * 512;
    if (type === 'x' || type === 'g') {
      const attrs = pax(bytes);
      if (type === 'g' && (attrs.path || attrs.size)) throw new Error('更新 tar 全局路径不支持。');
      if (type === 'x') extended = attrs;
      continue;
    }
    let name = extended.path || [cstring(header.subarray(345, 500)), cstring(header.subarray(0, 100))].filter(Boolean).join('/');
    if (extended.size && Number(extended.size) !== size) throw new Error('更新 tar 扩展大小不符。');
    extended = {};
    if (type === '5') {
      name = name.replace(/\/$/, ''); safeRelative(name);
      if (name !== folder && !name.startsWith(folder + '/')) throw new Error('更新包目录不符。');
    } else if (type === '0' || type === '\0') add(files, name, bytes, mode, folder, totals);
    else throw new Error('更新包不允许链接或特殊文件。');
  }
  if (!ended || !files.size) throw new Error('更新 tar 不完整。');
  return files;
}
export function unpack(archive, plan) {
  return plan.name.endsWith('.zip') ? zip(archive, plan.folder) : tar(archive, plan.folder);
}
