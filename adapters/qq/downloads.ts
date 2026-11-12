// URL与平台签名只留在适配器。主动下载后才返回本机文件；从不按卡片自动取内容。
import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import type { PhoneDownload } from "../../contracts/phone.ts";
import type { ModelImage } from "../../contracts/model.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";

function mime(bytes: Uint8Array): string | undefined {
  if (
    bytes.length >= 8 &&
    bytes.subarray(0, 8).every((b, i) => b === [137, 80, 78, 71, 13, 10, 26, 10][i])
  ) return "image/png";
  if (bytes.length >= 3 && bytes[0] === 255 && bytes[1] === 216 && bytes[2] === 255) {
    return "image/jpeg";
  }
  const text = new TextDecoder().decode(bytes.subarray(0, 12));
  if (text.startsWith("GIF87a") || text.startsWith("GIF89a")) return "image/gif";
  if (text.startsWith("RIFF") && text.slice(8, 12) === "WEBP") return "image/webp";
}
async function hashFile(path: string): Promise<string> {
  const hash = createHash("sha256");
  const file = await Deno.open(path, { read: true });
  for await (const bytes of file.readable) hash.update(bytes);
  return hash.digest("hex");
}
export class QqPhoneDownloads {
  readonly journal: FactJournal;
  #saved = new Map<string, PhoneDownload>();
  #running = new Map<string, Promise<PhoneDownload>>();
  #closed = new AbortController();
  constructor(readonly directory: string, readonly testOrigin?: string) {
    if (testOrigin && !/^http:\/\/127\.0\.0\.1:\d+$/.test(testOrigin)) {
      throw new TypeError("测试来源无效");
    }
    this.journal = new FactJournal(`${directory}/receipts`);
  }
  get available() {
    return this.journal.available && !this.#closed.signal.aborted;
  }
  async initialize() {
    await Deno.mkdir(`${this.directory}/files`, { recursive: true, mode: 0o700 });
    if (
      (await Deno.lstat(this.directory)).isSymlink ||
      (await Deno.lstat(`${this.directory}/files`)).isSymlink
    ) {
      throw new KernelError("corrupt", "附件目录不是独立目录，原件保留");
    }
    await this.journal.initialize();
    try {
      for (const event of this.journal.events) {
        if (event.kind !== "download") throw new KernelError("corrupt", "附件回执异常");
        const saved = event.data as unknown as PhoneDownload;
        if (
          !saved || !/^[a-f0-9]{64}$/.test(saved.sha256) ||
          !Number.isSafeInteger(saved.sizeBytes) || saved.sizeBytes < 0 ||
          saved.path !==
            `${this.directory}/files/${
              createHash("sha256").update(saved.source?.attachmentId ?? "").digest("hex")
            }` ||
          typeof saved.mimeType !== "string" || saved.source?.kind !== "phone_attachment" ||
          this.#saved.has(saved.source.attachmentId)
        ) throw new KernelError("corrupt", "附件回执异常，原件保留");
        this.#saved.set(saved.source.attachmentId, saved);
      }
    } catch (error) {
      await this.journal.close();
      throw error;
    }
  }
  #url(value: string): URL {
    let url: URL;
    try {
      url = new URL(value.startsWith("//") ? `https:${value}` : value);
    } catch {
      throw new KernelError("rejected", "附件没有可用下载地址");
    }
    const official = ["qq.com", "qpic.cn", "gtimg.cn", "qlogo.cn"].some((host) =>
      url.hostname === host || url.hostname.endsWith(`.${host}`)
    );
    if (
      url.username || url.password || url.hash ||
      !(url.protocol === "https:" && (!url.port || url.port === "443") && official) &&
        !(this.testOrigin && url.origin === this.testOrigin)
    ) throw new KernelError("rejected", "附件地址不属于平台资源来源");
    return url;
  }
  async #verify(saved: PhoneDownload) {
    let stat: Deno.FileInfo;
    try {
      stat = await Deno.lstat(saved.path);
    } catch {
      throw new KernelError("corrupt", "已下载附件缺失，原回执保留");
    }
    if (
      !stat.isFile || stat.isSymlink || stat.size !== saved.sizeBytes ||
      await hashFile(saved.path) !== saved.sha256
    ) {
      throw new KernelError("corrupt", "已下载附件内容发生变化，原件保留");
    }
    return structuredClone(saved);
  }
  async download(source: PhoneDownload["source"], rawUrl: string) {
    if (!this.available) throw new KernelError("unavailable", "附件下载不可用");
    const saved = this.#saved.get(source.attachmentId);
    if (saved) return await this.#verify(saved);
    const running = this.#running.get(source.attachmentId);
    if (running) return await running;
    const task = this.#download(structuredClone(source), rawUrl);
    this.#running.set(source.attachmentId, task);
    try {
      return await task;
    } finally {
      this.#running.delete(source.attachmentId);
    }
  }
  async #download(source: PhoneDownload["source"], rawUrl: string): Promise<PhoneDownload> {
    let url = this.#url(rawUrl);
    const signal = AbortSignal.any([this.#closed.signal, AbortSignal.timeout(120_000)]);
    let response: Response | undefined;
    for (let redirects = 0; redirects <= 5; redirects++) {
      try {
        response = await fetch(url, { redirect: "manual", signal });
      } catch {
        throw new KernelError("remote", "附件下载连接失败，未宣称已保存");
      }
      if (![301, 302, 303, 307, 308].includes(response.status)) break;
      await response.body?.cancel();
      const location = response.headers.get("location");
      if (!location || redirects === 5) throw new KernelError("remote", "附件下载重定向无效");
      url = this.#url(new URL(location, url).href);
    }
    if (!response?.ok || !response.body) {
      await response?.body?.cancel();
      throw new KernelError("remote", "平台未提供附件内容，可能已过期");
    }
    const path = `${this.directory}/files/${
      createHash("sha256").update(source.attachmentId).digest("hex")
    }`;
    // 每次失败保留独立部分文件；没有完整回执的旧文件从不当作成功。
    const partial = `${path}.${crypto.randomUUID()}.part`;
    const file = await Deno.open(partial, { createNew: true, write: true, mode: 0o600 });
    const hash = createHash("sha256");
    let sizeBytes = 0;
    let first = new Uint8Array();
    try {
      for await (const chunk of response.body) {
        if (first.length < 16) {
          const joined = new Uint8Array(Math.min(16, first.length + chunk.length));
          joined.set(first);
          joined.set(chunk.subarray(0, joined.length - first.length), first.length);
          first = joined;
        }
        hash.update(chunk);
        sizeBytes += chunk.length;
        let offset = 0;
        while (offset < chunk.length) {
          const written = await file.write(chunk.subarray(offset));
          if (!written) throw new Error();
          offset += written;
        }
      }
      await file.sync();
    } catch {
      throw new KernelError("storage", "附件未完整保存，部分文件保留");
    } finally {
      file.close();
    }
    try {
      // 原有孤立完整文件也保留，避免覆盖尚待核对的唯一原件。
      await Deno.link(partial, path);
      await Deno.remove(partial);
      const directory = await Deno.open(`${this.directory}/files`, { read: true });
      try {
        await directory.sync();
      } finally {
        directory.close();
      }
    } catch {
      throw new KernelError("storage", "附件提交失败，原件保留");
    }
    const result: PhoneDownload = {
      path,
      sizeBytes,
      sha256: hash.digest("hex"),
      mimeType: mime(first) ?? "application/octet-stream",
      source,
    };
    await this.journal.append("download", result as unknown as Json);
    this.#saved.set(source.attachmentId, result);
    return structuredClone(result);
  }
  async image(download: PhoneDownload): Promise<{ image: ModelImage; download: PhoneDownload }> {
    const saved = await this.#verify(download);
    if (!saved.mimeType.startsWith("image/")) {
      throw new KernelError("rejected", "附件不是受支持的图片格式，原文件仍已下载");
    }
    if (saved.sizeBytes > 8 * 1024 * 1024) {
      throw new KernelError(
        "rejected",
        "图片超过单次模型图像输入大小，请先用文件工具缩放，原文件仍已下载",
      );
    }
    const bytes = await Deno.readFile(saved.path);
    return {
      image: {
        dataUrl: `data:${saved.mimeType};base64,${Buffer.from(bytes).toString("base64")}`,
        provenance: {
          ...saved.source,
          sha256: saved.sha256,
          mimeType: saved.mimeType as ModelImage["provenance"]["mimeType"],
        },
      },
      download: saved,
    };
  }
  async readBytes(path: string): Promise<Uint8Array | undefined> {
    const saved = [...this.#saved.values()].find((saved) => saved.path === path);
    if (!saved) return undefined;
    await this.#verify(saved);
    // 上传接口采用内存JSON；不把无限大小附件直接装入一个请求。
    if (saved.sizeBytes > 32 * 1024 * 1024) {
      throw new KernelError("rejected", "附件超过单次上传读取大小，原下载文件仍保留");
    }
    const bytes = await Deno.readFile(path);
    if (createHash("sha256").update(bytes).digest("hex") !== saved.sha256) {
      throw new KernelError("corrupt", "附件读取期间发生变化，原件保留");
    }
    return bytes;
  }
  async file(
    download: PhoneDownload,
    args: { offsetBytes?: number; limitBytes?: number; encoding?: "utf8" | "base64" },
  ) {
    const saved = await this.#verify(download);
    const offset = args.offsetBytes ?? 0, limit = args.limitBytes ?? 16_384;
    if (
      !Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
      limit > 65_536 ||
      !["utf8", "base64"].includes(args.encoding ?? "utf8")
    ) throw new KernelError("rejected", "附件分页参数无效");
    const file = await Deno.open(saved.path, { read: true });
    const bytes = new Uint8Array(limit);
    let size = 0;
    try {
      await file.seek(offset, Deno.SeekMode.Start);
      while (size < limit) {
        const count = await file.read(bytes.subarray(size));
        if (count === null) break;
        size += count;
      }
    } finally {
      file.close();
    }
    return {
      download: saved,
      offsetBytes: offset,
      text: args.encoding === "base64"
        ? Buffer.from(bytes.subarray(0, size)).toString("base64")
        : new TextDecoder().decode(bytes.subarray(0, size)),
      ...(offset + size < saved.sizeBytes ? { nextOffsetBytes: offset + size } : {}),
    };
  }
  async close() {
    this.#closed.abort();
    await Promise.allSettled([...this.#running.values()]);
    await this.journal.close();
  }
}
