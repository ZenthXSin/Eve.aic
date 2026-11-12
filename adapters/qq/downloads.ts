// URL与平台签名只留在适配器。下载回执私有；临时原件可供 Eve 使用并定期清理。
import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import { isAbsolute, normalize } from "node:path";
import type { PhoneDownload } from "../../contracts/phone.ts";
import type { ModelImage } from "../../contracts/model.ts";
import type { PhoneAttachmentFailure } from "../../contracts/phone-rpc.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { type ErrorKind, type Json, KernelError } from "../../kernel/protocol.ts";

export class QqAttachmentDownloadError extends KernelError {
  readonly attachment: PhoneAttachmentFailure;
  constructor(kind: ErrorKind, reason: PhoneAttachmentFailure["reason"], httpStatus?: number) {
    super(
      kind,
      `${
        reason === "source_rejected" || reason === "redirect_rejected"
          ? "附件来源不符合下载限制"
          : "附件下载未完成"
      }：${reason}，未宣称已保存`,
    );
    this.attachment = {
      operation: "attachment_download",
      reason,
      ...(httpStatus === undefined ? {} : { httpStatus }),
    };
  }
}

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
  constructor(
    readonly directory: string,
    readonly testOrigin?: string,
    readonly timeoutMs = 30_000,
    readonly options: {
      temporaryDirectory?: string;
      identity?: { uid: number; gid: number };
      maxBytes?: number;
    } = {},
  ) {
    if (testOrigin && !/^http:\/\/127\.0\.0\.1:\d+$/.test(testOrigin)) {
      throw new TypeError("测试来源无效");
    }
    // 下载先于手机 RPC 的 120 秒期限终结，避免超时只剩 unknown。
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0 || timeoutMs > 90_000) {
      throw new TypeError("附件下载期限无效");
    }
    if (
      options.temporaryDirectory !== undefined &&
        (!isAbsolute(options.temporaryDirectory) ||
          normalize(options.temporaryDirectory) !== options.temporaryDirectory ||
          options.temporaryDirectory === "/" || options.temporaryDirectory === directory ||
          options.temporaryDirectory.startsWith(`${directory}/`)) ||
      !Number.isSafeInteger(this.maxBytes) || this.maxBytes < 1 ||
      options.identity &&
        (!Number.isSafeInteger(options.identity.uid) || options.identity.uid < 0 ||
          !Number.isSafeInteger(options.identity.gid) || options.identity.gid < 0)
    ) throw new TypeError("附件临时目录或容量限制无效");
    this.journal = new FactJournal(`${directory}/receipts`);
  }
  get maxBytes() {
    return this.options.maxBytes ?? 64 * 1024 * 1024;
  }
  #path(attachmentId: string, temporary = !!this.options.temporaryDirectory) {
    return `${temporary ? this.options.temporaryDirectory : `${this.directory}/files`}/${
      createHash("sha256").update(attachmentId).digest("hex")
    }`;
  }
  #temporary(saved: PhoneDownload) {
    return !!this.options.temporaryDirectory &&
      saved.path === this.#path(saved.source?.attachmentId ?? "");
  }
  #sameSource(a: PhoneDownload["source"], b: PhoneDownload["source"]) {
    return a.kind === b.kind && a.conversationId === b.conversationId &&
      a.sourceSeq === b.sourceSeq && a.attachmentId === b.attachmentId &&
      a.senderPlatformUserId === b.senderPlatformUserId;
  }
  hasReceipt(attachmentId: string) {
    return this.#saved.has(attachmentId);
  }
  /** 面板投影只检查位置、类型与大小；真正使用原件时仍校验完整摘要。 */
  async local(attachmentId: string): Promise<PhoneDownload | undefined> {
    const saved = this.#saved.get(attachmentId);
    return saved ? await this.#verify(saved, true, false) : undefined;
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
    if (this.options.temporaryDirectory) {
      const temporary = this.options.temporaryDirectory;
      await Deno.mkdir(temporary, { recursive: true, mode: 0o700 });
      const resolved = await Deno.realPath(temporary);
      if (resolved !== temporary || !(await Deno.lstat(temporary)).isDirectory) {
        throw new KernelError("corrupt", "附件临时目录不是独立目录");
      }
      await Deno.chmod(temporary, 0o700);
      if (this.options.identity) {
        await Deno.chown(temporary, this.options.identity.uid, this.options.identity.gid);
      }
      // 正在下载的文件不在可清理的根目录中，且不向 Eve 开放目录内容。
      const inflight = `${temporary}/.inflight`;
      await Deno.mkdir(inflight, { recursive: true, mode: 0o700 });
      const inflightStat = await Deno.lstat(inflight);
      if (!inflightStat.isDirectory || inflightStat.isSymlink) {
        throw new KernelError("corrupt", "附件下载中目录不是独立目录");
      }
      await Deno.chmod(inflight, 0o700);
      const { uid, gid } = await Deno.lstat(this.directory);
      if (uid !== null && gid !== null) await Deno.chown(inflight, uid, gid);
    }
    await this.journal.initialize();
    try {
      for (const event of this.journal.events) {
        if (event.kind !== "download") throw new KernelError("corrupt", "附件回执异常");
        const saved = event.data as unknown as PhoneDownload;
        const previous = this.#saved.get(saved?.source?.attachmentId);
        if (
          !saved || !/^[a-f0-9]{64}$/.test(saved.sha256) ||
          !Number.isSafeInteger(saved.sizeBytes) || saved.sizeBytes < 0 ||
          (saved.path !== this.#path(saved.source?.attachmentId ?? "", false) &&
            !this.#temporary(saved)) ||
          typeof saved.mimeType !== "string" || saved.source?.kind !== "phone_attachment" ||
          typeof saved.source.attachmentId !== "string" || !saved.source.attachmentId ||
          typeof saved.source.conversationId !== "string" ||
          !Number.isSafeInteger(saved.source.sourceSeq) || saved.source.sourceSeq < 1 ||
          typeof saved.source.senderPlatformUserId !== "string" ||
          previous && !(this.#temporary(previous) && this.#temporary(saved) &&
              this.#sameSource(previous.source, saved.source))
        ) throw new KernelError("corrupt", "附件回执异常，原件保留");
        this.#saved.set(saved.source.attachmentId, saved);
      }
      // 成功持有本下载器日志锁后，才清理上次异常退出的临时 part。
      if (this.options.temporaryDirectory) {
        for await (const entry of Deno.readDir(`${this.options.temporaryDirectory}/.inflight`)) {
          if (entry.isFile && /^[a-f0-9-]{36}\.part$/.test(entry.name)) {
            await Deno.remove(`${this.options.temporaryDirectory}/.inflight/${entry.name}`);
          }
        }
      }
    } catch (error) {
      await this.journal.close();
      throw error;
    }
  }
  #url(value: string, reason: "source_rejected" | "redirect_rejected" = "source_rejected"): URL {
    let url: URL;
    try {
      url = new URL(value.startsWith("//") ? `https:${value}` : value);
    } catch {
      throw new QqAttachmentDownloadError("rejected", reason);
    }
    // QQ 开放平台当前入站附件来自这个独立的官方多媒体主机。
    // 只补充精确主机，不把整个 qq.com.cn 或相似后缀纳入允许范围。
    const official = url.hostname === "multimedia.nt.qq.com.cn" ||
      ["qq.com", "qpic.cn", "gtimg.cn", "qlogo.cn"].some((host) =>
        url.hostname === host || url.hostname.endsWith(`.${host}`)
      );
    if (
      url.username || url.password || url.hash ||
      !(url.protocol === "https:" && (!url.port || url.port === "443") && official) &&
        !(this.testOrigin && url.origin === this.testOrigin)
    ) throw new QqAttachmentDownloadError("rejected", reason);
    return url;
  }
  #verify(saved: PhoneDownload): Promise<PhoneDownload>;
  #verify(
    saved: PhoneDownload,
    missingTemporary: true,
    verifyHash?: boolean,
  ): Promise<PhoneDownload | undefined>;
  async #verify(saved: PhoneDownload, missingTemporary = false, verifyHash = true) {
    let stat: Deno.FileInfo;
    try {
      stat = await Deno.lstat(saved.path);
    } catch (error) {
      if (missingTemporary && this.#temporary(saved) && error instanceof Deno.errors.NotFound) {
        return;
      }
      throw new KernelError("corrupt", "已下载附件缺失，原回执保留");
    }
    if (
      !stat.isFile || stat.isSymlink || stat.size !== saved.sizeBytes ||
      verifyHash && await hashFile(saved.path) !== saved.sha256
    ) {
      throw new KernelError("corrupt", "已下载附件内容发生变化，原件保留");
    }
    return structuredClone(saved);
  }
  async download(source: PhoneDownload["source"], rawUrl: string) {
    if (!this.available) throw new KernelError("unavailable", "附件下载不可用");
    const running = this.#running.get(source.attachmentId);
    if (running) return await running;
    const task = this.#reuseOrDownload(structuredClone(source), rawUrl);
    this.#running.set(source.attachmentId, task);
    try {
      return await task;
    } finally {
      this.#running.delete(source.attachmentId);
    }
  }
  async #reuseOrDownload(source: PhoneDownload["source"], rawUrl: string) {
    const saved = this.#saved.get(source.attachmentId);
    if (saved) {
      if (!this.#sameSource(saved.source, source)) {
        throw new KernelError("corrupt", "附件来源与原回执不符，原件保留");
      }
      const local = await this.#verify(saved, true);
      if (local) return local;
    }
    return await this.#download(source, rawUrl);
  }
  async #download(source: PhoneDownload["source"], rawUrl: string): Promise<PhoneDownload> {
    let url = this.#url(rawUrl);
    const timeout = AbortSignal.timeout(this.timeoutMs);
    const signal = AbortSignal.any([this.#closed.signal, timeout]);
    const networkFailure = (reason: "connection_failed" | "body_incomplete") =>
      this.#closed.signal.aborted
        ? new QqAttachmentDownloadError("unavailable", "closed")
        : timeout.aborted
        ? new QqAttachmentDownloadError("timeout", "timeout")
        : new QqAttachmentDownloadError("remote", reason);
    let response: Response | undefined;
    for (let redirects = 0; redirects <= 5; redirects++) {
      try {
        response = await fetch(url, { redirect: "manual", signal });
      } catch {
        throw networkFailure("connection_failed");
      }
      if (![301, 302, 303, 307, 308].includes(response.status)) break;
      await response.body?.cancel().catch(() => {});
      const location = response.headers.get("location");
      if (!location || redirects === 5) {
        throw new QqAttachmentDownloadError("remote", "redirect_invalid");
      }
      let next: URL;
      try {
        next = new URL(location, url);
      } catch {
        throw new QqAttachmentDownloadError("remote", "redirect_invalid");
      }
      url = this.#url(next.href, "redirect_rejected");
    }
    if (!response?.ok || !response.body) {
      await response?.body?.cancel().catch(() => {});
      throw new QqAttachmentDownloadError(
        "remote",
        response && !response.ok ? "http_rejected" : "empty_response",
        response && !response.ok ? response.status : undefined,
      );
    }
    const contentLength = response.headers.get("content-length");
    if (
      contentLength !== null && /^\d+$/.test(contentLength) && Number(contentLength) > this.maxBytes
    ) {
      await response.body.cancel().catch(() => {});
      throw new QqAttachmentDownloadError("rejected", "too_large");
    }
    const path = this.#path(source.attachmentId);
    // 临时附件与 part 同一文件系统；part 隐藏在适配器私有子目录，夜检仅清理根目录完成文件。
    const partial = this.options.temporaryDirectory
      ? `${this.options.temporaryDirectory}/.inflight/${crypto.randomUUID()}.part`
      : `${path}.${crypto.randomUUID()}.part`;
    let file: Deno.FsFile;
    try {
      file = await Deno.open(partial, { createNew: true, write: true, mode: 0o600 });
    } catch {
      await response.body.cancel().catch(() => {});
      throw new QqAttachmentDownloadError("storage", "storage_failed");
    }
    const hash = createHash("sha256");
    let sizeBytes = 0;
    let first = new Uint8Array();
    const reader = response.body.getReader();
    try {
      while (true) {
        let next: ReadableStreamReadResult<Uint8Array>;
        try {
          next = await reader.read();
        } catch {
          throw networkFailure("body_incomplete");
        }
        if (next.done) break;
        const chunk = next.value;
        if (chunk.length > this.maxBytes - sizeBytes) {
          throw new QqAttachmentDownloadError("rejected", "too_large");
        }
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
    } catch (error) {
      if (this.options.temporaryDirectory) {
        // 先关闭句柄再删除；旧版私有目录里的部分原件仍保留供诊断。
        file.close();
        await Deno.remove(partial).catch(() => {});
      }
      if (error instanceof QqAttachmentDownloadError) throw error;
      throw new QqAttachmentDownloadError("storage", "storage_failed");
    } finally {
      await reader.cancel().catch(() => {});
      reader.releaseLock();
      try {
        file.close();
      } catch { /* 失败清理已关闭 */ }
    }
    try {
      if (this.options.identity) {
        await Deno.chown(partial, this.options.identity.uid, this.options.identity.gid);
      }
      // 原有孤立完整文件也保留，避免覆盖尚待核对的唯一原件。
      await Deno.link(partial, path);
      await Deno.remove(partial);
      const directory = await Deno.open(
        this.options.temporaryDirectory ?? `${this.directory}/files`,
        { read: true },
      );
      try {
        await directory.sync();
      } finally {
        directory.close();
      }
    } catch {
      if (this.options.temporaryDirectory) await Deno.remove(partial).catch(() => {});
      throw new QqAttachmentDownloadError("storage", "storage_failed");
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
