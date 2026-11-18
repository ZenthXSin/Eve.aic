import { type BigIntStats, constants } from "node:fs";
import { lstat, open, realpath } from "node:fs/promises";
import { dirname, normalize } from "node:path/posix";
import { studioRecord, StudioSettingsError } from "../services/admin/studio-settings.ts";

export type StudioDocument = {
  path: string;
  data: Record<string, unknown>;
  version: string;
  uid: number;
  gid: number;
  mode: number;
};

const unavailable = () => new StudioSettingsError("unavailable");
const conflict = () => new StudioSettingsError("conflict");

function absolute(path: unknown): path is string {
  return typeof path === "string" && path.startsWith("/") && path.length <= 4096 &&
    !path.endsWith("/") && !path.includes("\0") && normalize(path) === path;
}

function version(info: BigIntStats) {
  return [info.dev, info.ino, info.ctimeNs, info.mtimeNs, info.size, info.mode, info.nlink].join(
    ":",
  );
}

function missing(error: unknown) {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}

async function trustedDirectory(path: string) {
  if (!absolute(path)) throw unavailable();
  const owner = Deno.permissions.querySync({ name: "sys", kind: "uid" }).state === "granted"
    ? Deno.uid() ?? 0
    : 0;
  let current = "";
  for (const part of path.slice(1).split("/")) {
    current += "/" + part;
    const info = await lstat(current);
    const stickyAncestor = info.uid === 0 && (info.mode & 0o1000) !== 0 && current !== path;
    if (
      !info.isDirectory() || (info.uid !== 0 && info.uid !== owner) ||
      ((info.mode & 0o022) !== 0 && !stickyAncestor)
    ) throw unavailable();
  }
  return owner;
}

function regular(info: BigIntStats, owner: number) {
  if (
    !info.isFile() || info.nlink !== 1n || (info.uid !== 0n && info.uid !== BigInt(owner)) ||
    (info.mode & 0o7022n) !== 0n
  ) throw unavailable();
}

export async function withStudioLock<T>(
  directory: string,
  exclusive: boolean,
  task: () => Promise<T>,
) {
  await trustedDirectory(directory);
  const handle = await Deno.open(directory, { read: true });
  try {
    await handle.lock(exclusive);
    return await task();
  } finally {
    handle.close();
  }
}

export async function readStudioDocument(
  path: string,
  limit: number,
): Promise<StudioDocument | undefined> {
  if (!absolute(path)) throw unavailable();
  try {
    const owner = await trustedDirectory(dirname(path));
    const before = await lstat(path, { bigint: true });
    regular(before, owner);
    if (before.size < 2n || before.size > BigInt(limit)) throw unavailable();
    const handle = await open(
      path,
      constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK,
    );
    try {
      const initial = version(before);
      if (version(await handle.stat({ bigint: true })) !== initial) throw conflict();
      const bytes = new Uint8Array(Number(before.size));
      let offset = 0;
      while (offset < bytes.length) {
        const { bytesRead } = await handle.read(bytes, offset, bytes.length - offset, null);
        if (!bytesRead) throw conflict();
        offset += bytesRead;
      }
      if (version(await lstat(path, { bigint: true })) !== initial) throw conflict();
      const data: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
      if (!studioRecord(data)) throw unavailable();
      return {
        path,
        data,
        version: initial,
        uid: Number(before.uid),
        gid: Number(before.gid),
        mode: Number(before.mode & 0o777n),
      };
    } finally {
      await handle.close();
    }
  } catch (error) {
    if (missing(error)) return undefined;
    throw error;
  }
}

export async function secretFileIsSet(ref: unknown): Promise<boolean> {
  if (!absolute(ref)) return false;
  try {
    const info = await lstat(ref);
    return info.isFile() && info.size > 0 && await realpath(ref) === ref;
  } catch (error) {
    if (missing(error)) return false;
    throw unavailable();
  }
}

export async function budgetSnapshotVersion(path: unknown) {
  if (!absolute(path)) return null;
  const owner = await trustedDirectory(dirname(path));
  try {
    const info = await lstat(path, { bigint: true });
    regular(info, owner);
    return version(info);
  } catch (error) {
    if (missing(error)) return null;
    throw error;
  }
}

export async function replaceStudioDocument(doc: StudioDocument, beforeRename: () => void) {
  const parent = dirname(doc.path);
  await trustedDirectory(parent);
  const temporary = `${parent}/.studio-settings-${crypto.randomUUID()}.tmp`;
  const handle = await open(temporary, "wx", 0o600);
  let renamed = false;
  try {
    try {
      await handle.writeFile(JSON.stringify(doc.data, null, 2) + "\n");
      const created = await handle.stat();
      if (created.uid !== doc.uid || created.gid !== doc.gid) await handle.chown(doc.uid, doc.gid);
      await handle.chmod(doc.mode);
      await handle.sync();
    } finally {
      await handle.close();
    }
    if (version(await lstat(doc.path, { bigint: true })) !== doc.version) throw conflict();
    beforeRename();
    await Deno.rename(temporary, doc.path);
    renamed = true;
    const directory = await Deno.open(parent, { read: true });
    try {
      await directory.sync();
    } finally {
      directory.close();
    }
  } finally {
    if (!renamed) await Deno.remove(temporary);
  }
}
