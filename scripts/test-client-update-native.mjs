// 原生包验收专用：生产下载器仍校验固定 GitHub URL；仅在这里注入回环 HTTP 传输。
import assert from 'node:assert/strict';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
const [root, server, expectedFailure] = process.argv.slice(2);
const { createUpdater } = await import(pathToFileURL(path.join(root, 'Updater.mjs')));
const updater = createUpdater(root, { fetchImpl: (url, options) => fetch(server + (url.includes('/releases/latest') ? '/latest' : '/archive'), options) });
let failed = false;
try { await updater.check({ force: true }); } catch { failed = true; }
assert.equal(failed, expectedFailure === 'failure');
const saved = await updater.status();
assert.equal(Boolean(saved.pending), !failed);
console.log(JSON.stringify({ downloaded: !failed, pending: saved.pending?.version || null, failed }));
