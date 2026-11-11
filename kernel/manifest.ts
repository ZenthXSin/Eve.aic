// 读取并校验插件清单 manifest.json；不合法的清单在加载前拒绝。

import { KernelError, type Manifest } from "./protocol.ts";
import { parseVersion, satisfies } from "./semver.ts";

const DEFAULT_DEADLINES = { loadMs: 5000, enableMs: 5000, probeMs: 2000, disableMs: 3000 };
const DEFAULT_CONFIRM = { calls: 1, deadlineMs: 3000 };

function reject(path: string, message: string): never {
  throw new KernelError("rejected", `清单不合法（${path}）：${message}`);
}

function stringMap(value: unknown, path: string, field: string): Record<string, string> {
  if (value === undefined) return {};
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    reject(path, `${field} 必须是对象`);
  }
  const result: Record<string, string> = {};
  for (const [key, item] of Object.entries(value)) {
    if (typeof item !== "string") reject(path, `${field}.${key} 必须是字符串`);
    result[key] = item;
  }
  return result;
}

function stringList(value: unknown, path: string, field: string): string[] {
  if (value === undefined) return [];
  if (!Array.isArray(value) || value.some((item) => typeof item !== "string")) {
    reject(path, `${field} 必须是字符串数组`);
  }
  return value as string[];
}

export function parseManifest(raw: unknown, path: string): Manifest {
  if (typeof raw !== "object" || raw === null) reject(path, "不是对象");
  const data = raw as Record<string, unknown>;
  if (typeof data.name !== "string" || !/^[a-z][a-z0-9-]*$/.test(data.name)) {
    reject(path, "name 必须是小写字母开头的短横线名字");
  }
  if (typeof data.version !== "string") reject(path, "缺少 version");
  try {
    parseVersion(data.version);
  } catch (error) {
    reject(path, (error as Error).message);
  }
  const provides = stringMap(data.provides, path, "provides");
  for (const [contract, version] of Object.entries(provides)) {
    try {
      parseVersion(version);
    } catch {
      reject(path, `provides.${contract} 必须是确定的版本号`);
    }
  }
  const requires = stringMap(data.requires, path, "requires");
  const optional = stringMap(data.optional, path, "optional");
  for (const [contract, range] of Object.entries({ ...requires, ...optional })) {
    try {
      satisfies("0.0.0", range);
    } catch {
      reject(path, `对 ${contract} 的版本范围写法不支持：${range}`);
    }
  }
  const permissions = (data.permissions ?? {}) as Record<string, unknown>;
  const deadlines = { ...DEFAULT_DEADLINES, ...(data.deadlines as object ?? {}) };
  const confirm = { ...DEFAULT_CONFIRM, ...(data.confirm as object ?? {}) };
  for (const [key, value] of Object.entries({ ...deadlines, ...confirm })) {
    if (typeof value !== "number" || !(value >= 0)) reject(path, `${key} 必须是非负数`);
  }
  const stateVersion = data.stateVersion ?? 1;
  if (typeof stateVersion !== "number" || !Number.isInteger(stateVersion)) {
    reject(path, "stateVersion 必须是整数");
  }
  return {
    name: data.name,
    version: data.version,
    entry: typeof data.entry === "string" ? data.entry : "mod.ts",
    stateVersion,
    provides,
    requires,
    optional,
    permissions: {
      net: stringList(permissions.net, path, "permissions.net"),
      read: stringList(permissions.read, path, "permissions.read"),
      write: stringList(permissions.write, path, "permissions.write"),
    },
    deadlines,
    confirm,
  };
}

export async function readManifest(dir: string): Promise<Manifest> {
  const path = `${dir}/manifest.json`;
  let text: string;
  try {
    text = await Deno.readTextFile(path);
  } catch (error) {
    throw new KernelError("rejected", `读不到清单 ${path}：${(error as Error).message}`);
  }
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch (error) {
    reject(path, `不是合法的 JSON：${(error as Error).message}`);
  }
  return parseManifest(raw, path);
}
