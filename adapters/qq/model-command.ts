// 主人私聊的显式模型控制指令；原始入站与发送账本始终保留，网关不等待模型或发送。
import { KernelError } from "../../kernel/protocol.ts";
import { QqSendActions } from "./actions.ts";
import type { InboundRecord } from "./journal.ts";
import { writeAtomic } from "./journal.ts";

export interface QqModelControl {
  status(): Promise<{ current: { model: string; revision: number }; availableModels: string[] }>;
  select(actionId: string, model: string): Promise<{
    actionId: string;
    model: string;
    revision: number;
    changed: boolean;
  }>;
  receipt(actionId: string): ReturnType<QqModelControl["select"]>;
}
export interface QqModelCommandOptions {
  ownerPlatformUserId?: string;
  control?: QqModelControl;
}
interface CommandRecord {
  schema: 1;
  identity: string;
  inboundSeq: number;
  argument: string;
  phase: "started" | "selecting" | "finished";
  model?: string;
  availableModels?: string[];
  replySequence?: number;
  outcome?: "listed" | "selected" | "denied" | "invalid" | "failed" | "unknown";
  reply?: string;
}

/** 只读取正文的完整独立指令；附件、引用中的文字、自然语言都不触发。 */
export function qqModelCommand(record: InboundRecord): { argument: string } | undefined {
  if (record.message.message.attachments?.length) return undefined;
  const rawText = record.message.message.text.trim();
  // 官方群 @ 机器人事件保留正文原件；这里只去掉事件所指机器人提及前缀。
  const text = record.raw.t === "GROUP_AT_MESSAGE_CREATE"
    ? rawText.replace(/^<@!?[0-9]+>[ \t]*/, "")
    : rawText;
  const matched = /^\/model(?:[ \t]+([^\r\n]*))?$/.exec(text);
  if (!matched) return undefined;
  return { argument: (matched[1] ?? "").trim() };
}
function selectedReceipt(
  result: Awaited<ReturnType<QqModelControl["select"]>>,
  identity: string,
  model: string,
) {
  if (
    !result || result.actionId !== `qq_model_${identity}` || result.model !== model ||
    !Number.isSafeInteger(result.revision) || result.revision < 0 ||
    typeof result.changed !== "boolean"
  ) {
    throw new KernelError("unknown", "模型切换回执与原动作不匹配");
  }
  return `当前模型：${result.model}（版本 ${result.revision}）。后续在线与图片请求使用此模型。`;
}
function commandError(error: unknown): "failed" | "unknown" {
  return error instanceof KernelError &&
      ["rejected", "not_found", "unavailable"].includes(error.kind)
    ? "failed"
    : "unknown";
}

export class QqModelCommands {
  #records = new Map<string, CommandRecord>();
  #inbound = new Map<string, InboundRecord>();
  #baseline = 0;
  #initialized = false;
  #closing = false;
  #failed = false;
  #lock?: Deno.FsFile;
  #tail: Promise<void> = Promise.resolve();
  #replies = new Set<Promise<unknown>>();
  #queued = new Set<string>();
  #blockedReplies = new Set<string>();
  #unsubscribe?: () => void;
  constructor(
    readonly directory: string,
    readonly actions: QqSendActions,
    readonly options: QqModelCommandOptions = {},
  ) {}
  get available() {
    return this.#initialized && !this.#failed && !this.#closing;
  }
  async initialize(existing: InboundRecord[]) {
    if (this.#initialized) throw new KernelError("rejected", "模型指令已经初始化");
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    if ((await Deno.lstat(this.directory)).isSymlink) {
      throw new KernelError("corrupt", "模型指令目录不能是链接");
    }
    this.#lock = await Deno.open(`${this.directory}/writer.lock`, {
      create: true,
      read: true,
      write: true,
      mode: 0o600,
    });
    if (!await this.#lock.tryLock(true)) {
      this.#lock.close();
      this.#lock = undefined;
      throw new KernelError("unavailable", "模型指令已有写入者");
    }
    try {
      const files: string[] = [];
      for await (const entry of Deno.readDir(this.directory)) {
        if (
          !entry.isFile || !/^(writer\.lock|boundary\.json|[a-f0-9]{64}\.json)$/.test(entry.name)
        ) {
          throw new KernelError("corrupt", "模型指令有未完成原件");
        }
        const stat = await Deno.lstat(`${this.directory}/${entry.name}`);
        if (stat.isSymlink || stat.size > 65_536) {
          throw new KernelError("corrupt", "模型指令原件不合法");
        }
        files.push(entry.name);
      }
      if (files.includes("boundary.json")) {
        const boundary = JSON.parse(await Deno.readTextFile(`${this.directory}/boundary.json`));
        if (
          boundary.schema !== 1 || !Number.isSafeInteger(boundary.throughSeq) ||
          boundary.throughSeq < 0 ||
          boundary.ownerPlatformUserId !== (this.options.ownerPlatformUserId ?? null)
        ) throw new KernelError("corrupt", "模型指令边界或主人绑定已变化");
        this.#baseline = boundary.throughSeq;
      } else {
        if (files.length !== 1) throw new KernelError("corrupt", "模型指令缺少启用边界");
        this.#baseline = existing.at(-1)?.seq ?? 0;
        await writeAtomic(`${this.directory}/boundary.json`, {
          schema: 1,
          throughSeq: this.#baseline,
          ownerPlatformUserId: this.options.ownerPlatformUserId ?? null,
        });
      }
      for (const inbound of existing) this.#inbound.set(inbound.identity, inbound);
      for (const file of files.filter((name) => /^[a-f0-9]{64}\.json$/.test(name))) {
        const saved: CommandRecord = JSON.parse(
          await Deno.readTextFile(`${this.directory}/${file}`),
        );
        const inbound = this.#inbound.get(saved.identity);
        const parsed = inbound && qqModelCommand(inbound);
        if (
          saved.schema !== 1 || file !== `${saved.identity}.json` || !inbound || !parsed ||
          saved.inboundSeq !== inbound.seq || inbound.seq <= this.#baseline ||
          parsed.argument !== saved.argument ||
          !["started", "selecting", "finished"].includes(saved.phase) ||
          (saved.model !== undefined &&
            (typeof saved.model !== "string" ||
              !/^[a-zA-Z0-9][a-zA-Z0-9._:/@+-]{0,255}$/.test(saved.model))) ||
          (saved.replySequence !== undefined &&
            (!Number.isSafeInteger(saved.replySequence) || saved.replySequence < 1)) ||
          (saved.availableModels !== undefined &&
            (!Array.isArray(saved.availableModels) || saved.availableModels.length > 200 ||
              saved.availableModels.some((model) =>
                typeof model !== "string" ||
                !/^[a-zA-Z0-9][a-zA-Z0-9._:/@+-]{0,255}$/.test(model)
              ))) ||
          (saved.phase === "selecting" && typeof saved.model !== "string") ||
          (saved.outcome === "listed" && !Array.isArray(saved.availableModels)) ||
          (saved.phase === "finished" &&
            (typeof saved.reply !== "string" || saved.reply.length > 2000 ||
              !["listed", "selected", "denied", "invalid", "failed", "unknown"].includes(
                saved.outcome!,
              )))
        ) throw new KernelError("corrupt", "模型指令记录与真实入站不一致");
        this.#records.set(saved.identity, saved);
      }
      this.#initialized = true;
      this.#unsubscribe = this.actions.onQueueChange(() => this.#pumpReplies());
      for (const record of existing) this.offer(record);
    } catch (error) {
      this.#failed = true;
      this.#lock.close();
      this.#lock = undefined;
      throw error;
    }
  }
  handled(record: InboundRecord) {
    return this.#initialized && record.seq > this.#baseline && qqModelCommand(record) !== undefined;
  }
  /** 稳定的可再生视图标记，不改入站原件、不随异步状态更换时间线指纹。 */
  project(record: InboundRecord): InboundRecord {
    if (!this.handled(record)) return record;
    return {
      ...record,
      message: {
        ...record.message,
        notify: false,
        message: {
          ...record.message.message,
          text:
            `[系统 /model 指令，适配器已接管；无需 Eve 重复执行]\n${record.message.message.text}`,
        },
      },
    };
  }
  offer(record: InboundRecord): void {
    if (
      !this.handled(record) || this.#closing || this.#failed || this.#queued.has(record.identity)
    ) return;
    this.#inbound.set(record.identity, structuredClone(record));
    this.#queued.add(record.identity);
    this.#tail = this.#tail.then(() => this.#process(record)).catch(() => {
      this.#failed = true;
    }).finally(() => {
      this.#queued.delete(record.identity);
      this.#pumpReplies();
    });
  }
  async #save(record: CommandRecord) {
    try {
      await writeAtomic(`${this.directory}/${record.identity}.json`, record);
      this.#records.set(record.identity, record);
    } catch {
      this.#failed = true;
      throw new KernelError("storage", "模型指令提交失败，原件保留");
    }
  }
  async #process(inbound: InboundRecord) {
    if (this.#failed || this.#closing) return;
    let saved = this.#records.get(inbound.identity);
    if (!saved) {
      saved = {
        schema: 1,
        identity: inbound.identity,
        inboundSeq: inbound.seq,
        argument: qqModelCommand(inbound)!.argument,
        phase: "started",
      };
      await this.#save(saved);
    }
    if (saved.phase !== "finished") {
      const owner = this.options.ownerPlatformUserId;
      const allowed = !!owner && !!this.options.control &&
        inbound.message.destination.kind === "user" &&
        inbound.message.destination.openId === owner &&
        inbound.message.message.sender.platformUserId === owner;
      let outcome: CommandRecord["outcome"];
      let reply: string;
      if (!allowed) {
        outcome = "denied";
        reply = "模型切换仅限已绑定主人的 QQ 私聊；当前请求未获授权。";
      } else {
        try {
          const control = this.options.control!;
          if (saved.phase === "selecting") {
            // 进程或 HTTP 中断后只核对原动作；未找到回执也不能猜测没执行。
            let result: Awaited<ReturnType<QqModelControl["receipt"]>>;
            try {
              result = await control.receipt(`qq_model_${inbound.identity}`);
            } catch {
              throw new KernelError("unknown", "模型切换回执尚未确认");
            }
            outcome = "selected";
            reply = selectedReceipt(result, inbound.identity, saved.model!);
          } else if (
            saved.argument && !/^[a-zA-Z0-9][a-zA-Z0-9._:/@+-]{0,255}$/.test(saved.argument)
          ) {
            outcome = "invalid";
            reply = "用法：/model 查看列表，/model 模型名 或 /model 序号 切换。";
          } else {
            const status = await control.status();
            const available = [...new Set(status.availableModels)].sort();
            if (!saved.argument) {
              outcome = "listed";
              const displayed: string[] = [];
              let characters = 0;
              for (const model of available) {
                characters += model.length + 8;
                if (characters > 1400) break;
                displayed.push(model);
              }
              saved = { ...saved, availableModels: displayed };
              reply = `当前模型：${status.current.model}\n可用模型：\n${
                displayed.map((model, index) => `${index + 1}. ${model}`).join("\n")
              }${
                available.length > displayed.length
                  ? `\n其余 ${available.length - displayed.length} 项可使用完整模型名切换。`
                  : ""
              }\n用 /model 模型名 或 /model 序号 切换。`;
            } else {
              // 序号绑定此前实际展示给主人且已持久化的列表，不用变化后的新顺序猜。
              const previousList = [...this.#records.values()].filter((record) =>
                record.outcome === "listed" && record.inboundSeq < inbound.seq &&
                this.#inbound.get(record.identity)?.message.destination.openId === owner &&
                (() => {
                  const receipt = this.actions.status(`qq_model_reply_${record.identity}`);
                  return receipt?.status === "finished" && receipt.result.status === "sent";
                })()
              ).sort((a, b) => b.inboundSeq - a.inboundSeq)[0]?.availableModels;
              const model = /^\d+$/.test(saved.argument)
                ? previousList?.[Number(saved.argument) - 1]
                : available.find((name) => name === saved!.argument);
              if (!model) {
                outcome = "invalid";
                reply = "未找到该模型或序号。请先用 /model 查看当前可用列表。";
              } else {
                saved = { ...saved, phase: "selecting", model };
                await this.#save(saved);
                const result = await control.select(`qq_model_${inbound.identity}`, model);
                outcome = "selected";
                reply = selectedReceipt(result, inbound.identity, saved.model!);
              }
            }
          }
        } catch (error) {
          // 存储失败不能写一份猜测的终结记录覆盖未完成现场。
          if (this.#failed) throw error;
          outcome = commandError(error);
          reply = outcome === "unknown"
            ? "模型切换结果尚未确认；已保留原动作，请勿当作切换成功。用 /model 可查询当前模型。"
            : "模型指令未完成，未确认切换。请稍后用 /model 查询当前模型。";
        }
      }
      saved = { ...saved, phase: "finished", outcome, reply };
      await this.#save(saved);
    }
    if (this.#closing) return;
    // 与 Eve 共用发送账本和会话 FIFO。即使命令 HTTP 已返回，第一条未成功也不能越过。
    const job = this.actions.send(`qq_model_reply_${inbound.identity}`, {
      destination: inbound.message.destination,
      text: saved.reply!,
    });
    this.#replies.add(job);
    void job.then((status) => {
      if (status.status === "blocked") {
        this.#blockedReplies.add(inbound.identity);
        // 关闭屏障与本次 blocked 回执可能交错，立即复核一次避免漏掉唤醒。
        this.#pumpReplies();
      } else this.#blockedReplies.delete(inbound.identity);
    }).catch(() => {
      if (!this.#closing) this.#failed = true;
    }).finally(() => this.#replies.delete(job));
  }
  #pumpReplies() {
    if (!this.available) return;
    for (const identity of this.#blockedReplies) {
      if (this.#queued.has(identity)) continue;
      const status = this.actions.status(`qq_model_reply_${identity}`);
      if (status?.status === "blocked") continue;
      this.#blockedReplies.delete(identity);
      if (status?.status === "cancelled") continue;
      const inbound = this.#inbound.get(identity);
      if (inbound) this.offer(inbound);
    }
  }
  async idle() {
    await this.#tail;
  }
  receipt(identity: string) {
    const record = this.#records.get(identity);
    return record && structuredClone(record);
  }
  requestStop() {
    this.#closing = true;
    this.#unsubscribe?.();
    this.#unsubscribe = undefined;
  }
  async close() {
    this.requestStop();
    await this.#tail;
    // 服务关闭先 requestStop()，因此持续超时链在此可结束。
    await Promise.allSettled([...this.#replies]);
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed) throw new KernelError("storage", "模型指令需核对原件");
  }
}
