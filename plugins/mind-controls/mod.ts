import { MIND, type MindEvent } from "../../contracts/mind.ts";
import { canonical } from "../../kernel/calls.ts";
import { definePlugin, type Json, KernelError } from "../../sdk/plugin.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

interface State {
  schema: 1;
  events: MindEvent[];
  alarms: { id: string; at: string; text: string; status: "pending" | "fired" | "cancelled" }[];
  facts: Json;
}
export function createMindControls(directory: string) {
  let state: State = { schema: 1, events: [], alarms: [], facts: null };
  let lock: Deno.FsFile | undefined;
  let failed = false;
  let tail: Promise<unknown> = Promise.resolve();
  const change = async (edit: (copy: State) => void) => {
    const operation = tail.then(async () => {
      if (!lock || failed) throw new KernelError("storage", "意识请求未保存");
      const next = structuredClone(state);
      edit(next);
      try {
        await writeDurableJson(`${directory}/index.json`, next);
      } catch {
        failed = true;
        throw new KernelError("storage", "意识请求存盘失败，原件保留");
      }
      state = next;
    });
    tail = operation.catch(() => {});
    await operation;
  };
  const request = async (kind: MindEvent["kind"], args: Json) => {
    if (
      args === null || typeof args !== "object" || Array.isArray(args) ||
      typeof args.requestId !== "string" || !args.requestId || args.requestId.length > 256
    ) throw new KernelError("rejected", "意识请求需要稳定编号");
    const id = args.requestId;
    const clean = structuredClone(args);
    delete clean.requestId;
    if (
      kind === "rest" &&
      (clean.wakeAt !== undefined &&
        (typeof clean.wakeAt !== "string" || !Number.isFinite(Date.parse(clean.wakeAt))))
    ) throw new KernelError("rejected", "休息时间无效");
    if (
      kind === "remind" &&
      (typeof clean.at !== "string" || !Number.isFinite(Date.parse(clean.at)) ||
        typeof clean.text !== "string" || !clean.text)
    ) throw new KernelError("rejected", "提醒需要时间与内容");
    if (kind === "cancelReminder" && typeof clean.reminderId !== "string") {
      throw new KernelError("rejected", "需要提醒编号");
    }
    if (
      kind === "compress" &&
      (!Number.isSafeInteger(clean.throughSeq) || Number(clean.throughSeq) < 1 ||
        typeof clean.summary !== "string" || !clean.summary)
    ) throw new KernelError("rejected", "压缩需要完整片段位置与自写摘要");
    if (
      kind === "retain" &&
      (!Number.isSafeInteger(clean.responseSeq) || Number(clean.responseSeq) < 1 ||
        !Number.isSafeInteger(clean.toolIndex) || Number(clean.toolIndex) < 0 ||
        !["raw", "summary", "omit"].includes(String(clean.mode)) ||
        (clean.mode === "summary" && typeof clean.summary !== "string"))
    ) throw new KernelError("rejected", "结果保留方式无效");
    await change((next) => {
      const old = next.events.find((event) => event.requestId === id);
      if (old) {
        if (old.kind !== kind || canonical(old.args) !== canonical(clean)) {
          throw new KernelError("rejected", "同一意识请求不能改参数");
        }
        return;
      }
      next.events.push({
        seq: next.events.length + 1,
        requestId: id,
        kind,
        args: clean,
        at: new Date().toISOString(),
      });
      if (kind === "remind") {
        next.alarms.push({
          id,
          at: clean.at as string,
          text: clean.text as string,
          status: "pending",
        });
      }
      if (kind === "cancelReminder") {
        const alarm = next.alarms.find((a) => a.id === clean.reminderId);
        if (!alarm) throw new KernelError("not_found", "没有该提醒");
        if (alarm.status === "pending") alarm.status = "cancelled";
      }
    });
    return { requestId: id, queued: true };
  };
  return definePlugin({
    load(ctx) {
      ctx.provide(MIND, {
        ...Object.fromEntries(
          ["rest", "remind", "cancelReminder", "compress", "retain"].map((
            kind,
          ) => [kind, (args: Json) => request(kind as MindEvent["kind"], args)]),
        ),
        status: () => state.facts,
        update: async (args) => {
          await change((next) => {
            next.facts = args !== null && typeof args === "object" && !Array.isArray(args) &&
                next.facts !== null && typeof next.facts === "object" && !Array.isArray(next.facts)
              ? { ...next.facts, ...args }
              : args;
          });
          return null;
        },
        poll: async (args) => {
          const after = (args as { afterSeq?: number } | null)?.afterSeq ?? 0;
          if (!Number.isSafeInteger(after) || after < 0 || after > state.events.length) {
            throw new KernelError("rejected", "意识事件游标无效");
          }
          const due = state.alarms.filter((alarm) =>
            alarm.status === "pending" && Date.parse(alarm.at) <= Date.now()
          );
          if (due.length) {
            await change((next) => {
              for (const alarm of next.alarms) {
                if (alarm.status !== "pending" || Date.parse(alarm.at) > Date.now()) continue;
                alarm.status = "fired";
                next.events.push({
                  seq: next.events.length + 1,
                  requestId: `alarm_${alarm.id}`,
                  kind: "alarm",
                  at: new Date().toISOString(),
                  args: {
                    reminderId: alarm.id,
                    text: alarm.text,
                    scheduledAt: alarm.at,
                    lateMs: Math.max(0, Date.now() - Date.parse(alarm.at)),
                  },
                });
              }
            });
          }
          const items = state.events.slice(after, after + 8);
          return {
            items: structuredClone(items) as unknown as Json,
            throughSeq: items.at(-1)?.seq ?? after,
            hasMore: after + items.length < state.events.length,
          };
        },
      });
    },
    async enable(ctx) {
      await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
      lock = await Deno.open(`${directory}/writer.lock`, {
        create: true,
        read: true,
        write: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) {
        lock.close();
        lock = undefined;
        throw new KernelError("unavailable", "已有意识控制实例");
      }
      ctx.onDisable(async () => {
        await tail;
        lock?.close();
        lock = undefined;
        if (failed) throw new KernelError("storage", "意识控制保存失败");
      });
      for await (const file of Deno.readDir(directory)) {
        if (!file.isFile || !["writer.lock", "index.json"].includes(file.name)) {
          throw new KernelError("corrupt", "意识控制存在未完成文件");
        }
      }
      try {
        const stat = await Deno.lstat(`${directory}/index.json`);
        if (!stat.isFile || stat.isSymlink || stat.size > 134_217_728) {
          throw new KernelError("corrupt", "意识控制原件异常");
        }
        const loaded = JSON.parse(await Deno.readTextFile(`${directory}/index.json`));
        if (
          loaded?.schema !== 1 || !Array.isArray(loaded.events) || !Array.isArray(loaded.alarms)
        ) throw new KernelError("corrupt", "意识控制结构损坏");
        const ids = new Set();
        for (const [i, event] of loaded.events.entries()) {
          if (
            event.seq !== i + 1 || typeof event.requestId !== "string" ||
            ids.has(event.requestId) ||
            !["rest", "remind", "cancelReminder", "compress", "retain", "alarm"].includes(
              event.kind,
            ) || !Number.isFinite(Date.parse(event.at))
          ) throw new KernelError("corrupt", "意识事件损坏");
          canonical(event.args);
          ids.add(event.requestId);
        }
        for (const alarm of loaded.alarms) {
          if (
            !ids.has(alarm.id) || !Number.isFinite(Date.parse(alarm.at)) ||
            typeof alarm.text !== "string" ||
            !["pending", "fired", "cancelled"].includes(alarm.status)
          ) throw new KernelError("corrupt", "提醒原件损坏");
        }
        canonical(loaded.facts);
        state = loaded;
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
    },
    health() {
      if (!lock || failed) throw new KernelError("unavailable", "意识控制不可用");
    },
  });
}
