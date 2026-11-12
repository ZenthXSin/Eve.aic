import { createHash } from "node:crypto";
import { dirname, isAbsolute, resolve } from "node:path";
import { KernelError } from "../../kernel/protocol.ts";

function digest(bytes: Uint8Array) {
  return createHash("sha256").update(bytes).digest("hex");
}
export class FileTools {
  #tail: Promise<unknown> = Promise.resolve();
  constructor(readonly root: string, readonly allowedRoots: string[] = [root]) {}
  async path(path: string, creating = false): Promise<string> {
    if (typeof path !== "string" || !path || path.includes("\0")) {
      throw new KernelError("rejected", "需要文件路径");
    }
    const full = resolve(this.root, path);
    let real: string;
    try {
      real = await Deno.realPath(full);
    } catch (e) {
      if (!(creating && e instanceof Deno.errors.NotFound)) throw e;
      real = resolve(await Deno.realPath(dirname(full)), full.slice(full.lastIndexOf("/") + 1));
    }
    const roots = await Promise.all(this.allowedRoots.map((r) => Deno.realPath(r)));
    if (!isAbsolute(real) || !roots.some((r) => real === r || real.startsWith(r + "/"))) {
      throw new KernelError("rejected", "路径不在当前配置的工作目录");
    }
    return real;
  }
  async read(args: { path: string; offsetBytes?: number; limitBytes?: number; encoding?: string }) {
    const path = await this.path(args.path),
      offset = args.offsetBytes ?? 0,
      limit = args.limitBytes ?? 16384;
    if (
      !Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
      limit > 65536 || !["utf8", "base64"].includes(args.encoding ?? "utf8")
    ) {
      throw new KernelError("rejected", "文件页参数无效");
    }
    const file = await Deno.open(path, { read: true });
    const hash = createHash("sha256"), parts: Uint8Array[] = [];
    let size = 0, shown = 0;
    try {
      const before = await file.stat();
      if (!before.isFile) throw new KernelError("rejected", "请读取普通文件");
      const buffer = new Uint8Array(65536);
      while (true) {
        const n = await file.read(buffer);
        if (n === null) break;
        hash.update(buffer.subarray(0, n));
        const start = Math.max(0, offset - size), end = Math.min(n, offset + limit - size);
        if (end > start) {
          parts.push(buffer.slice(start, end));
          shown += end - start;
        }
        size += n;
      }
      const after = await file.stat();
      if (after.size !== before.size || after.mtime?.getTime() !== before.mtime?.getTime()) {
        throw new KernelError("rejected", "读取期间文件改变，请重新读取");
      }
    } finally {
      file.close();
    }
    const bytes = new Uint8Array(shown);
    let at = 0;
    for (const part of parts) {
      bytes.set(part, at);
      at += part.length;
    }
    const text = args.encoding === "base64"
      ? btoa(String.fromCharCode(...bytes))
      : new TextDecoder().decode(bytes);
    return {
      path,
      sha256: hash.digest("hex"),
      sizeBytes: size,
      offsetBytes: offset,
      text,
      ...(offset + shown < size ? { nextOffsetBytes: offset + shown } : {}),
    };
  }
  async write(args: { path: string; text: string; expectedSha256: string }) {
    const captured = structuredClone(args);
    const work = this.#tail.then(() => this.#write(captured));
    this.#tail = work.catch(() => {});
    return await work;
  }
  async #write(args: { path: string; text: string; expectedSha256: string }) {
    if (
      typeof args.text !== "string" || new TextEncoder().encode(args.text).length > 4_194_304 ||
      !(args.expectedSha256 === "absent" || /^[a-f0-9]{64}$/.test(args.expectedSha256))
    ) {
      throw new KernelError("rejected", "写文件需要新内容及上次读取的 SHA256，新文件使用 absent");
    }
    const path = await this.path(args.path, true);
    let mode = 0o600;
    try {
      const previous = await this.read({ path, limitBytes: 1 });
      if (previous.sha256 !== args.expectedSha256) {
        throw new KernelError("rejected", "文件已改变，未覆盖");
      }
      mode = (await Deno.stat(path)).mode ?? 0o600;
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound && args.expectedSha256 === "absent")) throw error;
    }
    const bytes = new TextEncoder().encode(args.text),
      temporary = `${path}.${crypto.randomUUID()}.tmp`;
    const file = await Deno.open(temporary, { write: true, createNew: true, mode });
    try {
      let at = 0;
      while (at < bytes.length) {
        const n = await file.write(bytes.subarray(at));
        if (!n) throw new Error("写入未推进");
        at += n;
      }
      await file.sync();
    } finally {
      file.close();
    }
    await Deno.rename(temporary, path);
    const dir = await Deno.open(dirname(path), { read: true });
    try {
      await dir.sync();
    } finally {
      dir.close();
    }
    return { path, sha256: digest(bytes), sizeBytes: bytes.length };
  }
  async replace(args: { path: string; oldText: string; newText: string; expectedSha256: string }) {
    const captured = structuredClone(args);
    const work = this.#tail.then(async () => {
      const path = await this.path(captured.path);
      if (
        typeof captured.oldText !== "string" || !captured.oldText ||
        typeof captured.newText !== "string"
      ) throw new KernelError("rejected", "替换需要非空原文和新文");
      const original = await Deno.readTextFile(path);
      if (
        digest(new TextEncoder().encode(original)) !== captured.expectedSha256 ||
        original.split(captured.oldText).length !== 2
      ) {
        throw new KernelError("rejected", "原文须恰好出现一次且文件版本一致，未覆盖");
      }
      return await this.#write({
        path,
        expectedSha256: captured.expectedSha256,
        text: original.replace(captured.oldText, () => captured.newText),
      });
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  async list(args: { path?: string; afterName?: string } = {}) {
    const path = await this.path(args.path ?? "."), entries = [];
    for await (const e of Deno.readDir(path)) {
      if (args.afterName === undefined || e.name > args.afterName) {
        entries.push({
          name: e.name,
          kind: e.isSymlink ? "symlink" : e.isDirectory ? "directory" : "file",
        });
      }
    }
    entries.sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
    return { items: entries.slice(0, 32), hasMore: entries.length > 32 };
  }
  async search(args: { path: string; query: string; afterLine?: number }) {
    if (
      typeof args.query !== "string" || !args.query || args.query.length > 512 ||
      !Number.isSafeInteger(args.afterLine ?? 0) || (args.afterLine ?? 0) < 0
    ) throw new KernelError("rejected", "搜索参数无效");
    const path = await this.path(args.path), file = await Deno.open(path);
    const reader = file.readable.pipeThrough(new TextDecoderStream()).getReader();
    const items: { line: number; text: string }[] = [];
    let line = 0, carry = "", hasMore = false;
    const consume = (text: string) => {
      line++;
      if (line <= (args.afterLine ?? 0) || !text.includes(args.query)) return;
      if (items.length === 16) {
        hasMore = true;
        return;
      }
      items.push({ line, text: Array.from(text).slice(0, 512).join("") });
    };
    try {
      while (!hasMore) {
        const next = await reader.read();
        if (next.done) {
          if (carry) consume(carry);
          break;
        }
        carry += next.value;
        const lines = carry.split("\n");
        carry = lines.pop()!;
        for (const text of lines) {
          consume(text);
          if (hasMore) break;
        }
        if (carry.length > 4_194_304) {
          throw new KernelError("rejected", "单行过大，请使用字节分页读取");
        }
      }
    } finally {
      await reader.cancel();
      reader.releaseLock();
    }
    return { items, throughLine: hasMore ? items.at(-1)!.line : line, hasMore };
  }
  async close() {
    await this.#tail;
  }
}
