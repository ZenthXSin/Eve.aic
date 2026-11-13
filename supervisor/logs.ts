import { Buffer } from "node:buffer";
import { type Json, KernelError } from "../kernel/protocol.ts";

export const LOG_UNITS = ["guardian", "runtime", "qq-adapter", "supervisor", "admin"] as const;
export interface LogSource {
  id: string;
  kind: "file" | "journal";
  bytes?: number;
}

/** Byte pagination preserves every byte, including UTF-8 split across page boundaries. */
export async function readLogFile(path: string, offset = 0, limit = 65536): Promise<Json> {
  if (
    !Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
    limit > 262144
  ) {
    throw new KernelError(
      "rejected",
      "日志偏移必须非负，每页1至262144字节；可连续读取到末尾，无总量上限",
    );
  }
  const stat = await Deno.lstat(path);
  if (!stat.isFile || stat.isSymlink) throw new KernelError("rejected", "日志源不是普通文件");
  const file = await Deno.open(path, { read: true });
  try {
    const totalBytes = (await file.stat()).size;
    await file.seek(offset, Deno.SeekMode.Start);
    const bytes = new Uint8Array(Math.min(limit, Math.max(0, totalBytes - offset)));
    let count = 0;
    while (count < bytes.length) {
      const read = await file.read(bytes.subarray(count));
      if (read === null) break;
      count += read;
    }
    return {
      encoding: "base64",
      data: Buffer.from(bytes.subarray(0, count)).toString("base64"),
      offset,
      nextOffset: offset + count,
      totalBytes,
      eof: offset + count >= totalBytes,
    };
  } finally {
    file.close();
  }
}

/** Read from the oldest retained journal entry, then continue using the exact journal cursor. */
export async function readServiceJournal(
  unit: string,
  cursor?: string,
  limit = 100,
): Promise<Json> {
  if (
    !LOG_UNITS.includes(unit as never) || !Number.isSafeInteger(limit) || limit < 1 ||
    limit > 1000 ||
    (cursor !== undefined &&
      (typeof cursor !== "string" || cursor.length > 4096 || /[\r\n\0]/.test(cursor)))
  ) {
    throw new KernelError("rejected", "日志服务或游标无效，每页1至1000条；可连续读全部历史");
  }
  const child = new Deno.Command("/usr/bin/journalctl", {
    args: [
      "--quiet",
      "--no-pager",
      "--output=json",
      "--all",
      "--unit=eve-rebuild-" + unit + ".service",
      ...(cursor ? ["--after-cursor=" + cursor] : []),
    ],
    env: { PATH: "/usr/bin:/bin", LC_ALL: "C.UTF-8" },
    clearEnv: true,
    stdout: "piped",
    stderr: "null",
  }).spawn();
  const reader = child.stdout.pipeThrough(new TextDecoderStream()).getReader();
  let pending = "", eof = false, nextCursor = cursor;
  const items: Json[] = [];
  let succeeded = false;
  try {
    while (items.length < limit) {
      const newline = pending.indexOf("\n");
      if (newline >= 0) {
        const line = pending.slice(0, newline);
        pending = pending.slice(newline + 1);
        if (!line) continue;
        const entry = JSON.parse(line);
        if (typeof entry.__CURSOR !== "string") {
          throw new KernelError("corrupt", "系统日志缺少游标");
        }
        nextCursor = entry.__CURSOR;
        items.push(entry);
        continue;
      }
      const part = await reader.read();
      if (part.done) {
        eof = true;
        break;
      }
      pending += part.value;
    }
  } finally {
    try {
      if (!eof) child.kill("SIGTERM");
    } catch { /* journalctl may already have exited */ }
    await reader.cancel();
    reader.releaseLock();
    succeeded = (await child.status).success;
  }
  if (eof && !succeeded) throw new KernelError("unavailable", "系统日志读取失败，请核对原游标");
  return { items, ...(nextCursor ? { nextCursor } : {}), eof };
}
