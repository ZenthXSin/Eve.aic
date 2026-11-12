import { fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";
import { createHash } from "node:crypto";
import { resolve } from "node:path";

/** root 交接不可穿过候选可布置的祖先链接；调用者须先停止候选进程。 */
async function actualDirectory(path: string, create = false) {
  if (!path.startsWith("/")) throw new KernelError("rejected", "交接需要绝对目录");
  const absolute = resolve(path);
  let current = "";
  for (const part of absolute.split("/").filter(Boolean)) {
    current += `/${part}`;
    try {
      const stat = await Deno.lstat(current);
      if (!stat.isDirectory || stat.isSymlink) {
        throw new KernelError("rejected", "交接目录的任一祖先不可为链接或特殊文件");
      }
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound) || !create) throw error;
      await Deno.mkdir(current, { mode: 0o700 });
    }
  }
}
async function removeTemporary(path: string) {
  try {
    await Deno.remove(path);
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) throw error;
  }
}

/** 旧实例停用后，将提醒/控制游标交给尚未启用的新目录；不复制锁，不覆盖候选已有状态。 */
async function handoverState(
  sourceDirectory: string,
  targetDirectory: string,
  actionId: string,
  kind: "mind" | "scripts",
) {
  if (
    !sourceDirectory.startsWith("/") || !targetDirectory.startsWith("/") ||
    !actionId || sourceDirectory.replace(/\/$/, "") === targetDirectory.replace(/\/$/, "")
  ) {
    throw new KernelError("rejected", "提醒交接需要不同的绝对目录与稳定编号");
  }
  await actualDirectory(sourceDirectory);
  await actualDirectory(targetDirectory, true);
  const sourceStat = await Deno.lstat(sourceDirectory);
  if (!sourceStat.isDirectory || sourceStat.isSymlink) {
    throw new KernelError("rejected", "交接来源必须是实际目录，不能是符号链接");
  }
  const sourceReal = await Deno.realPath(sourceDirectory);
  const targetStat = await Deno.lstat(targetDirectory);
  if (!targetStat.isDirectory || targetStat.isSymlink) {
    throw new KernelError("rejected", "交接目标必须是实际目录，不能是符号链接");
  }
  const targetReal = await Deno.realPath(targetDirectory);
  if (
    sourceReal === targetReal || sourceReal.startsWith(`${targetReal}/`) ||
    targetReal.startsWith(`${sourceReal}/`)
  ) throw new KernelError("rejected", "提醒交接不能覆盖原目录");
  const locks: Deno.FsFile[] = [];
  try {
    for (const directory of [sourceDirectory, targetDirectory]) {
      const lockPath = `${directory}/writer.lock`;
      try {
        const stat = await Deno.lstat(lockPath);
        if (!stat.isFile || stat.isSymlink) throw new KernelError("corrupt", "提醒锁原件异常");
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      const lock = await Deno.open(lockPath, {
        create: true,
        read: true,
        write: true,
        mode: 0o600,
      });
      locks.push(lock);
      if (!(await lock.tryLock(true))) {
        throw new KernelError("unavailable", "提醒交接前必须停止旧实例且候选尚未启用");
      }
    }
    for await (const entry of Deno.readDir(targetDirectory)) {
      if (!entry.isFile || !["writer.lock", "index.json"].includes(entry.name)) {
        throw new KernelError("corrupt", "候选提醒目录有未确认文件，保留原件");
      }
    }
    const read = async (path: string) => {
      const stat = await Deno.lstat(path);
      if (!stat.isFile || stat.isSymlink || stat.size > 134_217_728) {
        throw new KernelError("corrupt", "提醒快照原件异常");
      }
      return JSON.parse(await Deno.readTextFile(path)) as Json;
    };
    let state: Json;
    try {
      state = await read(`${sourceDirectory}/index.json`);
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
      state = kind === "mind"
        ? { schema: 1, events: [], alarms: [], facts: null }
        : { schema: 1, jobs: [], completions: [] };
    }
    if (
      state === null || typeof state !== "object" || Array.isArray(state) || state.schema !== 1 ||
      (kind === "mind"
        ? !Array.isArray(state.events) || !Array.isArray(state.alarms)
        : !Array.isArray(state.jobs) || !Array.isArray(state.completions))
    ) throw new KernelError("corrupt", "提醒快照结构无效");
    const digest = await fingerprint(state);
    const receiptPath = `${targetDirectory}.handover.json`;
    let receipt: Json | undefined;
    try {
      receipt = await read(receiptPath);
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    if (receipt) {
      const prior = receipt as {
        actionId: string;
        digest: string;
        sourceDirectory: string;
        targetDirectory: string;
      };
      if (
        prior.actionId !== actionId || prior.digest !== digest ||
        prior.sourceDirectory !== sourceReal || prior.targetDirectory !== targetReal
      ) throw new KernelError("rejected", "同一提醒交接不能更改来源或覆盖已有候选");
    }
    let existing: Json | undefined;
    try {
      existing = await read(`${targetDirectory}/index.json`);
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    if (existing !== undefined && await fingerprint(existing) !== digest) {
      throw new KernelError("rejected", "候选已写下不同提醒状态，保留候选原件");
    }
    if (existing === undefined) await writeDurableJson(`${targetDirectory}/index.json`, state);
    const result = { actionId, digest, sourceDirectory: sourceReal, targetDirectory: targetReal };
    if (!receipt) await writeDurableJson(receiptPath, result);
    return result;
  } finally {
    for (const lock of locks.reverse()) lock.close();
  }
}

export function handoverMind(sourceDirectory: string, targetDirectory: string, actionId: string) {
  return handoverState(sourceDirectory, targetDirectory, actionId, "mind");
}
/** 对已停止的脚本独立事实账本做同样交接；未知动作不会因此重放。 */
export async function handoverScripts(
  sourceDirectory: string,
  targetDirectory: string,
  actionId: string,
) {
  if (!sourceDirectory.startsWith("/") || !targetDirectory.startsWith("/") || !actionId) {
    throw new KernelError("rejected", "脚本交接需要绝对目录与稳定编号");
  }
  await actualDirectory(sourceDirectory);
  await actualDirectory(targetDirectory, true);
  const sourceStat = await Deno.lstat(sourceDirectory);
  if (!sourceStat.isDirectory || sourceStat.isSymlink) {
    throw new KernelError("rejected", "脚本来源必须是实际目录");
  }
  const targetStat = await Deno.lstat(targetDirectory);
  if (!targetStat.isDirectory || targetStat.isSymlink) {
    throw new KernelError("rejected", "脚本目标必须是实际目录");
  }
  const sourceReal = await Deno.realPath(sourceDirectory);
  const targetReal = await Deno.realPath(targetDirectory);
  if (
    sourceReal === targetReal || sourceReal.startsWith(`${targetReal}/`) ||
    targetReal.startsWith(`${sourceReal}/`)
  ) {
    throw new KernelError("rejected", "脚本交接目录不可重叠");
  }
  type Entry = { path: string; digest: string; size: number };
  const locks: Deno.FsFile[] = [];
  const acquire = async (path: string) => {
    try {
      const stat = await Deno.lstat(path);
      if (!stat.isFile || stat.isSymlink) throw new KernelError("corrupt", "账本锁异常");
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    const lock = await Deno.open(path, { create: true, read: true, write: true, mode: 0o600 });
    locks.push(lock);
    if (!(await lock.tryLock(true))) {
      throw new KernelError("unavailable", "交接前必须停止脚本账本持有者");
    }
  };
  const digestFile = async (path: string): Promise<{ digest: string; size: number }> => {
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink) throw new KernelError("corrupt", "账本包含非普通文件");
    const hash = createHash("sha256");
    const file = await Deno.open(path, { read: true });
    let size = 0;
    try {
      const buffer = new Uint8Array(64 * 1024);
      for (;;) {
        const read = await file.read(buffer);
        if (read === null) break;
        hash.update(buffer.subarray(0, read));
        size += read;
      }
    } finally {
      file.close();
    }
    return { digest: hash.digest("hex"), size };
  };
  const walk = async (root: string, prefix = "", holdLocks = false): Promise<Entry[]> => {
    const result: Entry[] = [];
    for await (const entry of Deno.readDir(`${root}/${prefix}`)) {
      const path = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isSymlink || (!entry.isDirectory && !entry.isFile)) {
        throw new KernelError("corrupt", "脚本账本含链接或特殊文件，保留原件");
      }
      if (entry.name === "writer.lock") {
        if (!entry.isFile) throw new KernelError("corrupt", "账本锁类型异常");
        if (holdLocks && prefix) await acquire(`${root}/${path}`);
        continue;
      }
      if (entry.isDirectory) result.push(...await walk(root, path, holdLocks));
      else result.push({ path, ...await digestFile(`${root}/${path}`) });
    }
    return result.sort((a, b) => a.path.localeCompare(b.path));
  };
  try {
    await acquire(`${sourceReal}/writer.lock`);
    await acquire(`${targetReal}/writer.lock`);
    const files = await walk(sourceReal, "", true);
    const existing = await walk(targetReal, "", true);
    const emptyState = { schema: 1, jobs: [], completions: [] };
    const emptyIndex = files.length === 0
      ? new TextEncoder().encode(JSON.stringify(emptyState) + "\n")
      : undefined;
    if (emptyIndex) {
      files.push({
        path: "index.json",
        digest: createHash("sha256").update(emptyIndex).digest("hex"),
        size: emptyIndex.length,
      });
    }
    const index = files.find((entry) => entry.path === "index.json");
    if (!index || index.size > 134_217_728) {
      throw new KernelError("corrupt", "脚本交接缺少有效索引");
    }
    const state = emptyIndex
      ? emptyState
      : JSON.parse(await Deno.readTextFile(`${sourceReal}/index.json`));
    if (
      !state ||
      (state.schema === 1
        ? !Array.isArray(state.jobs) || !Array.isArray(state.completions)
        : state.schema !== 2 || state.layout !== "content-addressed-journal-v1")
    ) {
      throw new KernelError("corrupt", "脚本交接索引结构无效");
    }
    const digest = await fingerprint(files as unknown as Json);
    const result = { actionId, digest, sourceDirectory: sourceReal, targetDirectory: targetReal };
    const receiptPath = `${targetReal}.handover.json`;
    let prior: unknown;
    try {
      const stat = await Deno.lstat(receiptPath);
      if (!stat.isFile || stat.isSymlink || stat.size > 134_217_728) {
        throw new KernelError("corrupt", "脚本交接清单异常");
      }
      prior = JSON.parse(await Deno.readTextFile(receiptPath));
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
    }
    const manifest = { ...result, files };
    if (
      prior !== undefined && await fingerprint(prior as Json) !==
        await fingerprint(manifest as unknown as Json)
    ) {
      throw new KernelError("rejected", "脚本交接来源或原件已变化");
    }
    const wanted = new Map(files.map((entry) => [entry.path, entry]));
    for (const entry of existing) {
      const expected = wanted.get(entry.path);
      if (!expected || entry.digest !== expected.digest || entry.size !== expected.size) {
        throw new KernelError("rejected", "候选脚本已有不同原件，拒绝覆盖");
      }
    }
    // 清单先持久提交，复制中断可按相同原件安全补齐；候选从未在此步骤启用。
    if (prior === undefined) await writeDurableJson(receiptPath, manifest);
    const present = new Set(existing.map((entry) => entry.path));
    const temporary = `${targetReal}.handover-part`;
    for (const entry of files) {
      if (present.has(entry.path)) continue;
      try {
        const stat = await Deno.lstat(temporary);
        if (!stat.isFile || stat.isSymlink) throw new KernelError("corrupt", "交接暂存异常");
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      try {
        if (emptyIndex && entry.path === "index.json") await Deno.writeFile(temporary, emptyIndex);
        else await Deno.copyFile(`${sourceReal}/${entry.path}`, temporary);
        await Deno.chmod(temporary, 0o600);
        const file = await Deno.open(temporary, { write: true });
        try {
          await file.sync();
        } finally {
          file.close();
        }
        const actual = await digestFile(temporary);
        if (actual.digest !== entry.digest || actual.size !== entry.size) {
          throw new KernelError("corrupt", "脚本原件在复制时变化");
        }
        const destination = `${targetReal}/${entry.path}`;
        const parent = destination.slice(0, destination.lastIndexOf("/"));
        await Deno.mkdir(parent, { recursive: true, mode: 0o700 });
        await Deno.rename(temporary, destination);
        const directory = await Deno.open(parent, { read: true });
        try {
          await directory.sync();
        } finally {
          directory.close();
        }
      } finally {
        await removeTemporary(temporary);
      }
    }
    return result;
  } finally {
    for (const lock of locks.reverse()) lock.close();
  }
}
