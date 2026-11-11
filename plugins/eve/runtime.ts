import type { ModelResponse, ToolSpec } from "../../contracts/model.ts";
import type { MindEvent } from "../../contracts/mind.ts";
import { canonical, fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import type { PluginContext } from "../../sdk/plugin.ts";
import { ConsciousJournal } from "./journal.ts";
import {
  accountingDay,
  blocks,
  budget,
  contextEstimate,
  mechanicalCompression,
  type PendingTool,
  pendingTools,
  render,
} from "./view.ts";

export interface EveConfig {
  directory: string;
  promptFile: string;
  sourceCommit: string;
  modelTimeoutMs: number;
  maxOutputTokens: number;
  pollMs: number;
  defaultSleepMs: number;
  dailyTokenBudget: number;
  timeZone?: string;
}
export class EveRuntime {
  readonly journal: ConsciousJournal;
  #prompt = "";
  #running = false;
  #closing = false;
  #busy?: Promise<void>;
  #timer?: ReturnType<typeof setTimeout>;
  #wake = 0;
  #phoneThrough = 0;
  #scriptThrough = 0;
  #mindThrough = 0;
  #failures = 0;
  #retryAt = 0;
  #lastError?: string;
  #restApplied = false;
  #phoneFresh = false;
  #alarmFresh = false;
  #contextBlocked = false;
  #fatal = false;
  constructor(readonly ctx: PluginContext, readonly config: EveConfig) {
    this.journal = new ConsciousJournal(config.directory);
  }
  async initialize() {
    this.#prompt = await Deno.readTextFile(this.config.promptFile);
    if (!this.#prompt.trim()) throw new KernelError("rejected", "需要稳定身份与真实能力说明");
    await this.journal.initialize();
    for (const event of this.journal.events) {
      const data = event.data as Record<string, Json>;
      if (event.kind === "notice") {
        if (data.source === "phone") this.#phoneThrough = Number(data.throughSeq);
        if (data.source === "scripts") this.#scriptThrough = Number(data.throughSeq);
        if ((Array.isArray(data.items) && data.items.length) || data.source === "reminder") {
          this.#wake = 0;
        }
      }
      if (event.kind === "mind_applied") {
        this.#mindThrough = Number(data.throughSeq);
        if (data.wakeAt !== undefined) this.#wake = Number(data.wakeAt);
      }
      if (event.kind === "rest") this.#wake = Number(data.wakeAt);
      if (event.kind === "wake") this.#wake = 0;
      if (event.kind === "channel_failure") this.#lastError = String(data.errorKind);
      if (event.kind === "channel_recovered") this.#lastError = undefined;
      if (event.kind === "model_failure") {
        this.#failures++;
        this.#retryAt = Number(data.retryAt);
      }
      if (event.kind === "model_response") {
        this.#failures = 0;
        this.#retryAt = 0;
        if ((data.response as unknown as ModelResponse).toolCalls.length) this.#wake = 0;
      }
    }
    const responded = new Set(
      this.journal.events.filter((e) =>
        e.kind === "model_response" || e.kind === "model_failure" || e.kind === "recovery"
      ).map((e) => (e.data as { requestId: string }).requestId),
    );
    for (const event of [...this.journal.events]) {
      if (
        event.kind === "model_start" &&
        !responded.has((event.data as { requestId: string }).requestId)
      ) {
        await this.journal.append("recovery", {
          requestId: (event.data as { requestId: string }).requestId,
          result: "模型请求在宿主中断时未确认；费用与结果未知，未重放任何工具动作",
        });
        // 未确认请求的预算预留保留。下一次新请求只在恢复通知/醒来且预算足够时发生。
      }
    }
    if (!this.journal.events.length) {
      await this.journal.append("activation", {
        sourceCommit: this.config.sourceCommit,
        reason: "新基座首次启动，没有导入旧聊天记忆",
      });
    }
    if (this.#failures) this.#wake = 0;
  }
  start() {
    if (this.#closing) throw new KernelError("unavailable", "意识流已停止");
    if (this.#running) return;
    this.#running = true;
    this.#schedule(0);
  }
  #schedule(delay: number) {
    if (!this.#running || this.#closing) return;
    clearTimeout(this.#timer);
    this.#timer = setTimeout(() => {
      void this.pulse().catch(() => {});
    }, delay);
  }
  pulse(): Promise<void> {
    if (this.#busy) return this.#busy;
    const work = this.#pulse();
    this.#busy = work;
    return work;
  }
  async #pulse() {
    try {
      await this.#step();
      if (this.#lastError) {
        await this.journal.append("channel_recovered", { previousError: this.#lastError });
        this.#lastError = undefined;
      }
    } catch (error) {
      const kind = error instanceof KernelError ? error.kind : "internal";
      if (error instanceof KernelError && (error.kind === "storage" || error.kind === "corrupt")) {
        this.#running = false;
        this.#fatal = true;
        this.#lastError = kind;
        throw error;
      }
      if (this.#lastError !== kind && this.journal.available) {
        await this.journal.append("channel_failure", {
          errorKind: kind,
          reason: "当前依赖不可用，未重放已执行动作",
        });
      }
      this.#lastError = kind;
    } finally {
      this.#busy = undefined;
      this.#schedule(this.config.pollMs);
    }
  }
  async #notices(): Promise<boolean> {
    this.#restApplied = false;
    this.#phoneFresh = false;
    this.#alarmFresh = false;
    let fresh = false;
    const phone = await this.ctx.call<Json>("phone", "notifications", {
      afterSeq: this.#phoneThrough,
    });
    const page = phone as unknown as { items: Json[]; throughSeq: number; hasMore: boolean };
    if (page.throughSeq > this.#phoneThrough) {
      // 同一事实提交同时包含游标；不先标领取再写经历。
      await this.journal.append("notice", {
        source: "phone",
        throughSeq: page.throughSeq,
        items: page.items,
      });
      this.#phoneThrough = page.throughSeq;
      if (page.items.length) {
        fresh = true;
        this.#phoneFresh = true;
        this.#wake = 0;
      }
    }
    const tasks = await this.ctx.call<Json>("scripts", "completions", {
      afterSeq: this.#scriptThrough,
    });
    const completion = tasks as unknown as { items: Json[]; throughSeq: number };
    if (completion.throughSeq > this.#scriptThrough) {
      const observed = new Map<string, string>();
      for (const event of this.journal.events) {
        if (event.kind === "tool_result") {
          const output = (event.data as { result?: { output?: string } }).result?.output;
          if (typeof output !== "string") continue;
          try {
            const summary = JSON.parse(output);
            if (
              typeof summary.jobId === "string" &&
              ["completed", "failed", "cancelled", "unknown"].includes(summary.status)
            ) observed.set(summary.jobId, summary.status);
          } catch { /* 非脚本结果原样保留 */ }
        }
      }
      const newItems = completion.items.filter((item) => {
        const job = (item as { job?: { jobId?: string; status?: string } }).job;
        return !job?.jobId || observed.get(job.jobId) !== job.status;
      });
      await this.journal.append("notice", {
        source: "scripts",
        throughSeq: completion.throughSeq,
        items: newItems,
        alreadyObserved: completion.items.length - newItems.length,
      });
      this.#scriptThrough = completion.throughSeq;
      if (newItems.length) {
        fresh = true;
        this.#wake = 0;
      }
    }
    const controls = await this.ctx.call<Json>("mind", "poll", { afterSeq: this.#mindThrough });
    const events = controls as unknown as { items: MindEvent[]; throughSeq: number };
    for (const event of events.items) {
      let wakeAt: number | undefined;
      let outcome: Json = { status: "applied" };
      if (event.kind === "alarm") {
        await this.journal.append("notice", {
          source: "reminder",
          event: event as unknown as Json,
        });
        fresh = true;
        this.#alarmFresh = true;
      } else if (event.kind === "rest") {
        this.#restApplied = true;
        wakeAt =
          event.args !== null && typeof event.args === "object" && !Array.isArray(event.args) &&
            typeof event.args.wakeAt === "string"
            ? Date.parse(event.args.wakeAt)
            : Date.now() + this.config.defaultSleepMs;
      } else if (event.kind === "compress") {
        const args = event.args as { throughSeq: number; summary: string };
        const complete = blocks(this.journal.events);
        if (complete.some((block) => block.throughSeq === args.throughSeq)) {
          await this.journal.append("compression", {
            throughSeq: args.throughSeq,
            summary: args.summary,
            source: "eve",
          });
        } else {outcome = {
            status: "rejected",
            reason: "只可移动完整消息和工具对，指定位置不是完整片段边界",
          };}
      } else if (event.kind === "retain") {
        const args = event.args as {
          responseSeq: number;
          toolIndex: number;
          mode: string;
          summary?: string;
        };
        const result = this.journal.events.find((e) =>
          e.kind === "tool_result" &&
          (e.data as { callId: string }).callId === `eve_${args.responseSeq}_${args.toolIndex}`
        );
        if (result) await this.journal.append("retain", event.args);
        else outcome = { status: "rejected", reason: "对应工具回执尚未确认，未修改当前视图" };
      }
      await this.journal.append("mind_applied", {
        throughSeq: event.seq,
        requestId: event.requestId,
        outcome,
        ...(wakeAt !== undefined ? { wakeAt } : {}),
      });
      this.#mindThrough = event.seq;
      if (wakeAt !== undefined) this.#wake = wakeAt;
    }
    if (events.items.length) {
      await this.ctx.call("mind", "update", {
        lastControls: this.journal.events.filter((event) => event.kind === "mind_applied").slice(-8)
          .map((event) => event.data),
      });
    }
    return fresh;
  }
  async #tool(pending: PendingTool) {
    const call = pending.call;
    if (!pending.started) {
      await this.journal.append("tool_start", {
        responseSeq: pending.responseSeq,
        index: pending.index,
        callId: call.id,
        call: call as unknown as Json,
      });
    }
    let result: Json;
    if (pending.started && call.name === "execute_script") {
      const jobId = await fingerprint(["script-job-v1", call.id]);
      try {
        const summary = await this.ctx.call<Json>("scripts", "status", { jobId });
        result = { status: "completed", output: JSON.stringify(summary), adopted: true };
        await this.journal.append("tool_result", { callId: call.id, result });
        return;
      } catch (error) {
        if (!(error instanceof KernelError && error.kind === "not_found")) throw error;
      }
    }
    try {
      result = await this.ctx.call<Json>("tools", "execute", call as unknown as Json, {
        timeoutMs: this.config.modelTimeoutMs,
      });
    } catch (error) {
      const wire = toWireError(error);
      result = {
        status: wire.kind === "unknown" || wire.kind === "timeout" || wire.kind === "storage"
          ? "unknown"
          : "failed",
        error: wire as unknown as Json,
        requestId: call.id,
      };
      if (wire.kind === "storage") throw error;
    }
    await this.journal.append("tool_result", { callId: call.id, result });
  }
  async #step() {
    if (this.#closing || !this.journal.available) return;
    // 每次工具批次先完成/恢复原决定，再处理通知；新的通知不打断当前动作。
    const pending = pendingTools(this.journal.events);
    if (pending.length) {
      for (const tool of pending) {
        if (this.#closing) return;
        await this.#tool(tool);
      }
      return;
    }
    const fresh = await this.#notices();
    if (this.#restApplied && !this.#phoneFresh && !this.#alarmFresh) return;
    if (fresh) {
      this.#wake = 0;
      await this.journal.append("wake", { reason: "新的未观察通知" });
    }
    if (this.#closing) return;
    if (Date.now() < this.#retryAt) return;
    if (!fresh && Date.now() < this.#wake) return;
    const tools = await this.ctx.call<Json>("tools", "list") as unknown as ToolSpec[];
    let messages = render(this.journal.events, this.#prompt);
    const compression = mechanicalCompression(this.journal.events, this.#prompt, tools);
    if (compression) {
      await this.journal.append("compression", { ...compression, source: "system" });
      messages = render(this.journal.events, this.#prompt);
    }
    const estimated = contextEstimate(messages, tools);
    this.#contextBlocked = estimated >= 500_000;
    if (this.#contextBlocked) {
      if (this.journal.events.at(-1)?.kind !== "context_wait") {
        await this.journal.append("context_wait", {
          estimatedTokens: estimated,
          reason: "固定身份或工具说明仍过大，不能移动锚定内容；保留原件并停止模型请求",
        });
      }
      return;
    }
    const day = accountingDay(this.config.timeZone);
    const usage = budget(this.journal.events, day);
    let reservation = new TextEncoder().encode(JSON.stringify({ messages, tools })).length +
      messages.length * 32 + this.config.maxOutputTokens;
    const state = {
      at: new Date().toISOString(),
      sourceCommit: this.config.sourceCommit,
      contextEstimatedTokens: estimated,
      contextReminder: estimated >= 275_000 ? "已达到275k提醒线，是否整理由你决定" : null,
      dailyBudget: this.config.dailyTokenBudget,
      knownNoncachedTokens: usage.knownNoncachedTokens,
      unknownReservedTokens: usage.unknownReservedTokens,
      safeRemaining: Math.max(0, this.config.dailyTokenBudget - usage.safetyTotal),
      pendingTools: 0,
    };
    await this.ctx.call("mind", "update", state as unknown as Json);
    if (usage.safetyTotal + reservation > this.config.dailyTokenBudget) {
      if (this.journal.events.at(-1)?.kind !== "budget_wait") {
        await this.journal.append("budget_wait", {
          day,
          known: usage.knownNoncachedTokens,
          unknownReserved: usage.unknownReservedTokens,
          reason: "每日预算安全余量不足，停止模型请求",
        });
      }
      return;
    }
    // 感知追加一次，避免时间/用量改动稳定身份。没有固定QQ开场或自动消息。
    const band = Math.floor(estimated / 25_000);
    const last = this.journal.events.filter((event) => event.kind === "perception").at(-1);
    if (
      !last || (last.data as { contextBand: number }).contextBand !== band ||
      (last.data as { day: string }).day !== day
    ) {
      await this.journal.append("perception", {
        contextBand: band,
        day,
        sourceCommit: this.config.sourceCommit,
        remainingBand: Math.floor(state.safeRemaining / 10_000_000),
        reminder: state.contextReminder,
      });
      messages = render(this.journal.events, this.#prompt);
    }
    // 感知本身也属于请求；不能在刚低于触线时追加后直接发出超线请求。
    if (contextEstimate(messages, tools) >= 500_000) {
      const finalCompression = mechanicalCompression(this.journal.events, this.#prompt, tools);
      if (finalCompression) {
        await this.journal.append("compression", { ...finalCompression, source: "system" });
        messages = render(this.journal.events, this.#prompt);
      }
      if (contextEstimate(messages, tools) >= 500_000) {
        this.#contextBlocked = true;
        await this.journal.append("context_wait", { reason: "最终完整请求仍超出估算触线" });
        return;
      }
    }
    reservation = new TextEncoder().encode(JSON.stringify({ messages, tools })).length +
      messages.length * 32 + this.config.maxOutputTokens;
    if (usage.safetyTotal + reservation > this.config.dailyTokenBudget) {
      await this.journal.append("budget_wait", { day, reason: "含感知的完整请求预算余量不足" });
      return;
    }
    const requestId = `model_${this.journal.events.length + 1}`;
    await this.journal.append("model_start", {
      requestId,
      day,
      reservation,
      estimatedTokens: contextEstimate(messages, tools),
      sourceCommit: this.config.sourceCommit,
    });
    let response: ModelResponse;
    try {
      response = await this.ctx.call<Json>(
        "model",
        "complete",
        { messages, tools } as unknown as Json,
        { timeoutMs: this.config.modelTimeoutMs },
      ) as unknown as ModelResponse;
      if (!response || typeof response.content !== "string" || !Array.isArray(response.toolCalls)) {
        throw new KernelError("rejected", "模型回复不符合当前契约");
      }
      const ids = new Set<string>();
      for (const call of response.toolCalls) {
        if (
          typeof call.id !== "string" || !call.id || ids.has(call.id) ||
          !tools.some((tool) => tool.name === call.name)
        ) throw new KernelError("rejected", "模型工具不符合真实能力契约");
        ids.add(call.id);
        canonical(call.args);
      }
      if (!response.toolCalls.length && !response.content.trim()) {
        throw new KernelError("rejected", "模型返回了空的结束结果");
      }
    } catch (error) {
      this.#failures++;
      this.#retryAt = Date.now() + Math.min(300_000, 1000 * 2 ** Math.min(this.#failures, 18));
      await this.journal.append("model_failure", {
        requestId,
        error: toWireError(error) as unknown as Json,
        retryAt: this.#retryAt,
      });
      return;
    }
    await this.journal.append("model_response", {
      requestId,
      response: response as unknown as Json,
    });
    this.#failures = 0;
    this.#retryAt = 0;
    if (response.toolCalls.length === 0) {
      this.#wake = Date.now() + this.config.defaultSleepMs;
      await this.journal.append("rest", {
        wakeAt: this.#wake,
        reason: "本步没有继续动作；默认长唤醒，Eve可改",
      });
    } else this.#wake = 0;
  }
  status() {
    return {
      running: this.#running,
      busy: this.#busy !== undefined,
      healthy: this.journal.available && !this.#closing && !this.#fatal,
      sourceCommit: this.config.sourceCommit,
      records: this.journal.events.length,
      phoneThroughSeq: this.#phoneThrough,
      scriptThroughSeq: this.#scriptThrough,
      mindThroughSeq: this.#mindThrough,
      wakeAt: this.#wake,
      budget: budget(this.journal.events, accountingDay(this.config.timeZone)),
      lastError: this.#lastError ?? null,
      contextBlocked: this.#contextBlocked,
    };
  }
  read(args: { afterSeq?: number }) {
    const after = args.afterSeq ?? 0;
    if (!Number.isSafeInteger(after) || after < 0 || after > this.journal.events.length) {
      throw new KernelError("rejected", "意识记录游标无效");
    }
    const page = this.journal.events.slice(after, after + 8);
    return {
      items: page.map((event) => ({
        seq: event.seq,
        at: event.at,
        kind: event.kind,
        digest: event.digest,
      })),
      throughSeq: page.at(-1)?.seq ?? after,
      hasMore: after + page.length < this.journal.events.length,
    };
  }
  async close() {
    this.#closing = true;
    this.#running = false;
    clearTimeout(this.#timer);
    try {
      await this.#busy;
    } finally {
      await this.journal.close();
    }
  }
}
