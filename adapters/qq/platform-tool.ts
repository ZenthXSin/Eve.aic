import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { open } from "node:fs/promises";
import { writeDurableJson } from "../../sdk/durable-json.ts";
import type { QqPlatformAccess, QqPlatformResult } from "./platform-client.ts";

export function platformDigest(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}

function incomplete(): QqPlatformResult {
  return { status: "unknown", reason: "incomplete" };
}

function absoluteParts(path: string) {
  if (typeof path !== "string" || !path.startsWith("/") || path.includes("\0")) {
    throw new TypeError("QQ 平台需要实体绝对路径");
  }
  const parts = path.slice(1).split("/");
  if (parts.some((part) => !part || part === "." || part === "..")) {
    throw new TypeError("QQ 平台需要实体绝对路径");
  }
  return parts;
}

async function trustedParents(path: string, owner: number) {
  let parent = "";
  for (const part of absoluteParts(path).slice(0, -1)) {
    parent += `/${part}`;
    const stat = await Deno.lstat(parent);
    if (
      !stat.isDirectory || (stat.uid !== owner && stat.uid !== 0) || stat.mode === null ||
      ((stat.mode & 0o022) !== 0 && (stat.mode & 0o1000) === 0)
    ) {
      throw new Error("QQ 平台父目录不可信");
    }
  }
}

async function privateDirectory(path: string, owner: number) {
  const stat = await Deno.lstat(path);
  if (!stat.isDirectory || stat.uid !== owner || stat.mode === null || (stat.mode & 0o077) !== 0) {
    throw new Error("QQ 平台审计目录必须是本账户的私有实体目录");
  }
}

export async function readPrivatePlatformFile(path: string, owner: number, limit: number) {
  if (!Number.isSafeInteger(limit) || limit < 0 || limit > 262144) {
    throw new TypeError("QQ 平台文件上限无效");
  }
  await trustedParents(path, owner);
  const entry = await Deno.lstat(path);
  if (
    !entry.isFile || entry.uid !== owner || entry.mode === null || (entry.mode & 0o077) !== 0 ||
    entry.nlink !== 1 || entry.size > limit
  ) throw new Error("QQ 平台文件不安全");
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const before = await file.stat();
    if (
      !before.isFile() || before.uid !== owner || (before.mode & 0o077) !== 0 ||
      before.nlink !== 1 ||
      before.dev !== entry.dev || before.ino !== entry.ino || before.size > limit
    ) {
      throw new Error("QQ 平台文件已变更");
    }
    const bytes = new Uint8Array(before.size + 1);
    let offset = 0;
    while (offset < bytes.length) {
      const { bytesRead } = await file.read(bytes, offset, bytes.length - offset, null);
      if (!bytesRead) break;
      offset += bytesRead;
    }
    const after = await file.stat();
    if (
      offset !== before.size || after.size !== before.size || after.ctimeMs !== before.ctimeMs ||
      after.mode !== before.mode || after.uid !== before.uid ||
      after.nlink !== 1
    ) {
      throw new Error("QQ 平台文件已变更");
    }
    return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(
      bytes.subarray(0, offset),
    );
  } finally {
    await file.close();
  }
}

async function syncDirectory(path: string) {
  using directory = await Deno.open(path, { read: true });
  await directory.sync();
}

async function markerInfo(path: string, owner: number) {
  const stat = await Deno.lstat(path);
  if (
    !stat.isFile || stat.size !== 0 || stat.uid !== owner || stat.mode === null ||
    (stat.mode & 0o077) !== 0 || stat.ino === null || stat.dev === null ||
    (stat.nlink !== 1 && stat.nlink !== 2)
  ) throw new Error("QQ 平台审计标记无效");
  return stat;
}

async function verifyStore(store: string, owner: number) {
  await privateDirectory(store, owner);
  const anchor = await markerInfo(`${store}.identity`, owner);
  const marker = await markerInfo(`${store}/.identity`, owner);
  if (
    anchor.nlink !== 2 || marker.nlink !== 2 || anchor.dev !== marker.dev ||
    anchor.ino !== marker.ino
  ) {
    throw new Error("QQ 平台审计目录已变更");
  }
}

async function openStore(directory: string, store: string, owner: number) {
  try {
    await markerInfo(`${store}.identity`, owner);
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) throw error;
    try {
      await Deno.mkdir(store, { mode: 0o700 });
    } catch (error) {
      if (!(error instanceof Deno.errors.AlreadyExists)) throw error;
    }
    await privateDirectory(store, owner);
    const marker = `${store}/.identity`;
    try {
      using file = await Deno.open(marker, { createNew: true, write: true, mode: 0o600 });
      await file.sync();
    } catch (error) {
      if (!(error instanceof Deno.errors.AlreadyExists)) throw error;
    }
    await markerInfo(marker, owner);
    try {
      await Deno.link(marker, `${store}.identity`);
    } catch (error) {
      if (!(error instanceof Deno.errors.AlreadyExists)) throw error;
    }
  }
  await verifyStore(store, owner);
  await syncDirectory(store);
  await syncDirectory(directory);
}

async function reserve(path: string, intent: unknown, owner: number): Promise<boolean> {
  const temporary = `${path}.${crypto.randomUUID()}.reserve`;
  using file = await Deno.open(temporary, { createNew: true, write: true, mode: 0o600 });
  try {
    if ((await file.stat()).uid !== owner) throw new Error("QQ 平台审计目录所有者不符");
    const bytes = new TextEncoder().encode(JSON.stringify(intent) + "\n");
    let offset = 0;
    while (offset < bytes.length) {
      const written = await file.write(bytes.subarray(offset));
      if (written === 0) throw new Error("QQ 平台预留未写入");
      offset += written;
    }
    await file.sync();
    try {
      await Deno.link(temporary, path);
    } catch (error) {
      if (error instanceof Deno.errors.AlreadyExists) return false;
      throw error;
    }
    return true;
  } finally {
    await Deno.remove(temporary);
  }
}

interface Entry {
  operation: string;
  access: QqPlatformAccess;
  fingerprint: string;
  key?: string;
}

async function recover(
  path: string,
  owner: number,
  recordId: string,
  entry: Entry,
  decode: (value: unknown) => QqPlatformResult | undefined,
): Promise<QqPlatformResult> {
  try {
    const record = JSON.parse(await readPrivatePlatformFile(path, owner, 32_768));
    if (
      record?.version !== 2 || record.recordId !== recordId || !timestamp(record.startedAt) ||
      typeof record.fingerprint !== "string" || !/^[a-f0-9]{64}$/.test(record.fingerprint)
    ) return incomplete();
    if (
      record.fingerprint !== entry.fingerprint || record.operation !== entry.operation ||
      record.access !== entry.access
    ) {
      return { status: "failed", reason: "idempotency_conflict" };
    }
    if (!timestamp(record.finishedAt)) return incomplete();
    const result = decode(record.result);
    if (!result) return incomplete();
    await syncDirectory(path.slice(0, path.lastIndexOf("/")));
    return result;
  } catch {
    return incomplete();
  }
}

function timestamp(value: unknown): boolean {
  return typeof value === "string" && value.length === 24 && Number.isFinite(Date.parse(value)) &&
    new Date(value).toISOString() === value;
}

export function platformLedger(directory: string) {
  absoluteParts(directory);
  const store = `${directory}/platform-operations`;
  return async (
    entry: Entry,
    perform: () => Promise<QqPlatformResult>,
    decode: (value: unknown) => QqPlatformResult | undefined,
  ): Promise<QqPlatformResult> => {
    try {
      const owner = Deno.uid();
      if (owner === null || !Number.isSafeInteger(owner) || owner < 0) {
        throw new Error("QQ 平台审计账户无效");
      }
      await trustedParents(directory, owner);
      await privateDirectory(directory, owner);
      await openStore(directory, store, owner);
      const file = entry.key === undefined
        ? `read-${crypto.randomUUID()}`
        : `key-${platformDigest(entry.key)}`;
      const path = `${store}/${file}.json`;
      const intent = {
        version: 2,
        recordId: file,
        operation: entry.operation,
        access: entry.access,
        fingerprint: entry.fingerprint,
        startedAt: new Date().toISOString(),
      };
      if (!await reserve(path, intent, owner)) {
        return entry.key === undefined
          ? incomplete()
          : await recover(path, owner, file, entry, decode);
      }
      await syncDirectory(store);
      const result = await perform();
      await verifyStore(store, owner);
      const receipt = entry.key === undefined && result.status === "ok"
        ? { status: result.status, httpStatus: result.httpStatus }
        : result;
      await writeDurableJson(path, {
        ...intent,
        finishedAt: new Date().toISOString(),
        result: receipt,
      });
      return result;
    } catch {
      return { status: "unknown", reason: "audit_unavailable" };
    }
  };
}
