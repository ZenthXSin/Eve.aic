import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import { resolve } from "node:path";
import { handoverMind, handoverScripts } from "./mind-handover.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

const LIMIT = 256 * 1024 * 1024;
const MAX_FILES = 100_000;
const CHUNK = 48 * 1024;
type Entry = { kind: "file"; path: string; size: number; digest: string };
type Roots = { mind: string; scripts: string };
function fail(message: string): never {
  throw new Error(`交接拒绝：${message}`);
}
const validPath = (path: unknown): path is string =>
  typeof path === "string" &&
  /^(mind\/index\.json|scripts\/(index\.json|legacy-index\.json|objects\/[a-f0-9]{64}\.json|commits\/events\/\d{16}\.json))$/
    .test(path);

async function directory(path: string, create = false) {
  if (!path.startsWith("/")) fail("目录须为绝对路径");
  let current = "";
  for (const part of resolve(path).split("/").filter(Boolean)) {
    current += `/${part}`;
    try {
      const stat = await Deno.lstat(current);
      if (!stat.isDirectory || stat.isSymlink) fail("目录祖先包含链接或特殊文件");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound) || !create) throw error;
      await Deno.mkdir(current, { mode: 0o700 });
    }
  }
}
async function hashFile(path: string) {
  const stat = await Deno.lstat(path);
  if (!stat.isFile || stat.isSymlink || stat.size > LIMIT) fail("原件类型或长度无效");
  const hash = createHash("sha256");
  let size = 0;
  const file = await Deno.open(path, { read: true });
  try {
    const bytes = new Uint8Array(CHUNK);
    for (;;) {
      const count = await file.read(bytes);
      if (count === null) break;
      size += count;
      if (size > LIMIT) fail("原件超过限额");
      hash.update(bytes.subarray(0, count));
    }
  } finally {
    file.close();
  }
  return { size, digest: hash.digest("hex") };
}
async function writeAll(file: { write(bytes: Uint8Array): Promise<number> }, bytes: Uint8Array) {
  let offset = 0;
  while (offset < bytes.length) {
    const count = await file.write(bytes.subarray(offset));
    if (!count) fail("写入未推进");
    offset += count;
  }
}
async function roots(configPath: string): Promise<Roots> {
  const config = JSON.parse(await Deno.readTextFile(configPath));
  if (typeof config.mindDirectory !== "string" || typeof config.scripts?.directory !== "string") {
    fail("配置缺少mindDirectory或scripts.directory");
  }
  const mind = resolve(config.mindDirectory), scripts = resolve(config.scripts.directory, "ledger");
  if (
    !config.mindDirectory.startsWith("/") || !config.scripts.directory.startsWith("/") ||
    mind === scripts || mind.startsWith(`${scripts}/`) || scripts.startsWith(`${mind}/`)
  ) {
    fail("原件目录必须为互不重叠的绝对路径");
  }
  return { mind, scripts };
}
async function lock(path: string, locks: Deno.FsFile[]) {
  try {
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink) fail("锁文件异常");
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) throw error;
  }
  const file = await Deno.open(path, { create: true, read: true, write: true, mode: 0o600 });
  locks.push(file);
  if (!await file.tryLock(true)) fail("原件仍被运行实例持有");
}
async function* lines(input: ReadableStream<Uint8Array>) {
  let pending = "", total = 0;
  const decoder = new TextDecoder("utf-8", { fatal: true });
  for await (const bytes of input) {
    total += bytes.length;
    if (total > 370 * 1024 * 1024) fail("传输超过限额");
    pending += decoder.decode(bytes, { stream: true });
    for (;;) {
      const end = pending.indexOf("\n");
      if (end < 0) break;
      if (end > 100_000) fail("协议行过长");
      const line = pending.slice(0, end);
      pending = pending.slice(end + 1);
      yield JSON.parse(line);
    }
    if (pending.length > 100_000) fail("协议行过长");
  }
  pending += decoder.decode();
  if (pending.length) fail("传输未完整结束");
}

/** 只在旧实例UID内读取原件，stdout必须仅承载本协议。 */
export async function exportSnapshot(
  configPath: string,
  actionId: string,
  emit: (line: string) => Promise<void>,
) {
  if (!actionId || actionId.length > 512) fail("编号无效");
  const paths = await roots(configPath);
  const locks: Deno.FsFile[] = [];
  const entries: Entry[] = [];
  const defaults = new Map<string, Uint8Array>();
  let total = 0;
  const visit = async (root: string, prefix: string, relative = "") => {
    for await (const entry of Deno.readDir(`${root}/${relative}`)) {
      const local = relative ? `${relative}/${entry.name}` : entry.name;
      const path = `${prefix}/${local}`;
      if (entry.isSymlink || (!entry.isFile && !entry.isDirectory)) fail("原件含链接或特殊文件");
      if (entry.name === "writer.lock") {
        if (!entry.isFile) fail("锁类型无效");
        if (relative) await lock(`${root}/${local}`, locks);
        continue;
      }
      if (entry.isDirectory) {
        if (!/^scripts\/(objects|commits|commits\/events)$/.test(path)) fail("原件含未知目录");
        await visit(root, prefix, local);
      } else {
        if (!validPath(path)) fail("原件含未知文件");
        const value = await hashFile(`${root}/${local}`);
        total += value.size;
        if (total > LIMIT || entries.length >= MAX_FILES) fail("原件总量超过限额");
        entries.push({ kind: "file", path, ...value });
      }
    }
  };
  const send = (value: unknown) => emit(JSON.stringify(value) + "\n");
  try {
    for (const path of Object.values(paths)) {
      await directory(path, true);
      await lock(`${path}/writer.lock`, locks);
    }
    for (const [kind, path] of Object.entries(paths)) {
      await visit(path, kind);
      if (!entries.some((entry) => entry.path === `${kind}/index.json`)) {
        if (entries.some((entry) => entry.path.startsWith(`${kind}/`))) fail("原件缺少索引");
        const state = kind === "mind"
          ? { schema: 1, events: [], alarms: [], facts: null }
          : { schema: 1, jobs: [], completions: [] };
        const bytes = new TextEncoder().encode(JSON.stringify(state) + "\n");
        defaults.set(`${kind}/index.json`, bytes);
        total += bytes.length;
        if (total > LIMIT) fail("原件总量超过限额");
        entries.push({
          kind: "file",
          path: `${kind}/index.json`,
          size: bytes.length,
          digest: createHash("sha256").update(bytes).digest("hex"),
        });
      }
    }
    entries.sort((a, b) => a.path.localeCompare(b.path));
    await send({ kind: "start", schema: 1, actionId });
    const manifestHash = createHash("sha256");
    for (const entry of entries) {
      manifestHash.update(JSON.stringify(entry) + "\n");
      await send(entry);
      const initial = defaults.get(entry.path);
      if (initial) await send({ kind: "chunk", data: Buffer.from(initial).toString("base64") });
      else {
        const [kind, ...parts] = entry.path.split("/");
        const file = await Deno.open(`${paths[kind as keyof Roots]}/${parts.join("/")}`, {
          read: true,
        });
        try {
          const bytes = new Uint8Array(CHUNK);
          for (;;) {
            const count = await file.read(bytes);
            if (count === null) break;
            await send({
              kind: "chunk",
              data: Buffer.from(bytes.subarray(0, count)).toString("base64"),
            });
          }
        } finally {
          file.close();
        }
      }
      await send({ kind: "file_end" });
    }
    await send({ kind: "end", digest: manifestHash.digest("hex") });
  } finally {
    for (const file of locks.reverse()) file.close();
  }
}

/** 候选UID内收取完整原件；完成摘要核验后才交入候选事实目录。 */
export async function importSnapshot(
  configPath: string,
  actionId: string,
  input: ReadableStream<Uint8Array>,
) {
  if (!actionId || actionId.length > 512) fail("编号无效");
  const paths = await roots(configPath);
  const staging = `${paths.scripts}.transfer-${
    createHash("sha256").update(actionId).digest("hex")
  }`;
  await directory(staging, true);
  const locks: Deno.FsFile[] = [];
  await lock(`${staging}/writer.lock`, locks);
  let part: Deno.FsFile | undefined, current: Entry | undefined;
  let size = 0, total = 0, started = false, ended = false;
  let fileHash = createHash("sha256");
  const manifestHash = createHash("sha256");
  const seen = new Set<string>();
  const temporary = `${staging}/part`;
  try {
    for await (const value of lines(input)) {
      if (ended || !value || typeof value !== "object") fail("协议顺序无效");
      if (!started) {
        if (value.kind !== "start" || value.schema !== 1 || value.actionId !== actionId) {
          fail("交接编号不符");
        }
        started = true;
        continue;
      }
      if (value.kind === "file") {
        if (
          current || !validPath(value.path) || seen.has(value.path) || seen.size >= MAX_FILES ||
          !Number.isSafeInteger(value.size) || value.size < 0 ||
          typeof value.digest !== "string" || !/^[a-f0-9]{64}$/.test(value.digest)
        ) fail("文件清单无效");
        total += value.size;
        if (total > LIMIT) fail("原件总量超过限额");
        current = { kind: "file", path: value.path, size: value.size, digest: value.digest };
        seen.add(current.path);
        manifestHash.update(JSON.stringify(current) + "\n");
        try {
          const stat = await Deno.lstat(temporary);
          if (!stat.isFile || stat.isSymlink) fail("暂存类型无效");
        } catch (error) {
          if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
        part = await Deno.open(temporary, {
          create: true,
          truncate: true,
          write: true,
          mode: 0o600,
        });
        size = 0;
        fileHash = createHash("sha256");
      } else if (value.kind === "chunk") {
        if (
          !part || !current || typeof value.data !== "string" || value.data.length > 65_536 ||
          !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value.data)
        ) fail("数据块无效");
        const bytes = Buffer.from(value.data, "base64");
        size += bytes.length;
        if (size > current.size) fail("文件长度超过清单");
        fileHash.update(bytes);
        await writeAll(part, bytes);
      } else if (value.kind === "file_end") {
        if (
          !part || !current || size !== current.size || fileHash.digest("hex") !== current.digest
        ) fail("原件摘要不符");
        await part.sync();
        part.close();
        part = undefined;
        const destination = `${staging}/${current.path}`;
        await directory(destination.slice(0, destination.lastIndexOf("/")), true);
        let exists = false;
        try {
          const prior = await hashFile(destination);
          if (prior.digest !== current.digest || prior.size !== current.size) {
            fail("同编号原件已变化");
          }
          exists = true;
        } catch (error) {
          if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
        if (exists) await Deno.remove(temporary);
        else await Deno.rename(temporary, destination);
        current = undefined;
      } else if (value.kind === "end") {
        if (
          current || seen.size < 2 || !seen.has("mind/index.json") ||
          !seen.has("scripts/index.json") ||
          value.digest !== manifestHash.digest("hex")
        ) fail("交接清单不完整");
        ended = true;
      } else fail("未知协议记录");
    }
    if (!ended) fail("交接流未完整结束");
    // staging不能混入上一次未完成传输之外的文件。
    const check = async (path: string, prefix = "") => {
      for await (const entry of Deno.readDir(path)) {
        if (entry.name === "writer.lock" && entry.isFile) continue;
        const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
        if (entry.isSymlink) fail("暂存含链接");
        if (entry.isDirectory) await check(`${path}/${entry.name}`, relative);
        else if (!entry.isFile || !seen.has(relative)) fail("暂存含未确认原件");
      }
    };
    await check(staging);
    const receipt = {
      actionId,
      mind: await handoverMind(`${staging}/mind`, paths.mind, actionId),
      scripts: await handoverScripts(`${staging}/scripts`, paths.scripts, `${actionId}_scripts`),
    };
    await writeDurableJson(`${staging}.receipt.json`, receipt);
    for (const file of locks.splice(0).reverse()) file.close();
    await Deno.remove(staging, { recursive: true });
    return receipt;
  } finally {
    part?.close();
    for (const file of locks.reverse()) file.close();
  }
}

if (import.meta.main) {
  const [config, mode, actionId] = Deno.args;
  try {
    if (!config || !actionId || !["export", "import"].includes(mode)) {
      fail("用法 CONFIG export|import ACTION_ID");
    }
    if (mode === "export") {
      await exportSnapshot(
        config,
        actionId,
        (line) => writeAll(Deno.stdout, new TextEncoder().encode(line)),
      );
    } else console.log(JSON.stringify(await importSnapshot(config, actionId, Deno.stdin.readable)));
  } catch (error) {
    console.error(error instanceof Error ? error.message : "交接失败");
    Deno.exit(1);
  }
}
