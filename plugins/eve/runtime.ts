import type { Message, ModelResponse, ToolSpec } from "../../contracts/model.ts";
import type { NoteApi } from "../../contracts/notes.ts";
import { presentContext } from "../../sdk/context-presentation.ts";
import type { MindEvent } from "../../contracts/mind.ts";
import { canonical, fingerprint } from "../../kernel/calls.ts";
import { type ErrorKind, type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import type { PluginContext } from "../../sdk/plugin.ts";
import { CONTEXT_TOOLS, inspectContext, previewCompression } from "./context-tools.ts";
import { recallQuery } from "./recall-query.ts";
import { type ModelBlock, providerRejection } from "../../sdk/model-failure.ts";
import { type ConsciousEvent, ConsciousJournal } from "./journal.ts";
import { RemoteJournal } from "./remote-journal.ts";
import { thoughtForCall, withoutThought, WRITE_THOUGHT_TOOL } from "./thoughts.ts";
import {
  accountingDay,
  activeEvents,
  blocks,
  budget,
  contextComposition,
  type ContextCounter,
  contextEstimate,
  lastActiveEvent,
  mechanicalCompression,
  type PendingTool,
  pendingTools,
  render,
} from "./view.ts";

export interface EveConfig {
  startupTimeoutMs?: number;
  directory: string;
  promptFile: string;
  sourceCommit: string;
  modelTimeoutMs: number;
  maxOutputTokens: number;
  pollMs: number;
  defaultSleepMs: number;
  dailyTokenBudget: number;
  timeZone?: string;
  remoteRecords?: boolean;
  agentsEnabled?: boolean;
}
export class EveRuntime {
  readonly journal: ConsciousJournal | RemoteJournal;
  #prompt = "";
  #selfMessages: Message[] = [];
  #selfIdeas: Awaited<ReturnType<NoteApi["self"]>>["ideas"] = [];
  async #selfContext(idle: boolean) {
    this.#selfMessages = [];
    this.#selfIdeas = [];
    if (!this.config.remoteRecords) return;
    try {
      const self = await this.ctx.call("notes", "self", { idle }) as unknown as Awaited<
        ReturnType<NoteApi["self"]>
      >;
      if (self?.aboutMe) {
        this.#selfMessages.push({
          role: "user",
          content:
            `你自己写的“关于我”（便签 ${self.aboutMe.id} v${self.aboutMe.revision}，${self.aboutMe.tokens} token，${self.aboutMe.encoding}）。这是自己的经历与偏好，固定身份与可信权限仍以系统设定为准：\n${self.aboutMe.content}`,
        });
      }
      if (self?.ideas?.length) {
        this.#selfIdeas = self.ideas;
        this.#selfMessages.push({
          role: "user",
          content: "独处时翻到自己写的几条原话，可以挑一件，也可以不理：\n" +
            self.ideas.map((idea) => `便签 ${idea.noteId} v${idea.revision}：\n${idea.text}`).join(
              "\n\n",
            ),
        });
      }
    } catch (error) {
      if (!(error instanceof KernelError && error.kind === "rejected")) throw error;
      this.#selfMessages.push({
        role: "user",
        content: `自我便签未挂载：${error.message}。可用 notes.get/update 核对和自行精简。`,
      });
    }
  }
  #presentationScanned = 0;
  #rawCalls = new Set<string>();
  #receiptRecords = new Map<string, number>();
  #present(messages: Message[]) {
    const events = this.journal.events;
    let rebuild = events.length < this.#presentationScanned;
    for (let i = this.#presentationScanned; i < events.length; i++) {
      if (events[i].kind === "upgrade_rollback") rebuild = true;
    }
    if (rebuild) {
      this.#rawCalls.clear();
      this.#receiptRecords.clear();
    }
    const source = rebuild ? activeEvents(events) : events;
    for (let i = rebuild ? 0 : this.#presentationScanned; i < source.length; i++) {
      const event = source[i];
      if (event.kind === "tool_result") {
        this.#receiptRecords.set((event.data as { callId: string }).callId, event.seq);
      }
      if (event.kind === "retain") {
        const d = event.data as { responseSeq: number; toolIndex: number; mode: string };
        const id = `eve_${d.responseSeq}_${d.toolIndex}`;
        if (d.mode === "raw") this.#rawCalls.add(id);
        else this.#rawCalls.delete(id);
      }
    }
    this.#presentationScanned = events.length;
    return [
      ...presentContext(messages, this.#rawCalls, this.#receiptRecords),
      ...this.#selfMessages,
    ];
  }
  #render() {
    return this.#present(render(this.journal.events, this.#prompt));
  }
  #running = false;
  #closing = false;
  #busy?: Promise<void>;
  #timer?: ReturnType<typeof setTimeout>;
  #toolStartedAt = new Map<string, number>();
  #continueImmediately = false;
  #observedJobs = new Map<string, { callId: string; status: string }>();
  #rememberJob(callId: string, result: Json) {
    try {
      const output = (result as { output?: string })?.output;
      if (typeof output !== "string") return;
      const summary = JSON.parse(output);
      if (typeof summary.jobId === "string" && typeof summary.status === "string") {
        this.#observedJobs.set(summary.jobId, { callId, status: summary.status });
      }
    } catch { /* 非脚本回执不建立任务索引。 */ }
  }
  async #saveToolResult(callId: string, result: Json) {
    const started = this.#toolStartedAt.get(callId);
    await this.journal.append("tool_result", {
      callId,
      sourceCommit: this.config.sourceCommit,
      result,
      ...(started === undefined
        ? {}
        : { executionMs: Math.round((performance.now() - started) * 100) / 100 }),
    });
    this.#toolStartedAt.delete(callId);
    this.#rememberJob(callId, result);
  }
  #wake = 0;
  #agentThrough = 0;
  #agentFresh = false;
  #phoneThrough = 0;
  #scriptThrough = 0;
  #commandThrough = 0;
  #imageActions = new Set<string>();
  #mindThrough = 0;
  #failures = 0;
  #retryAt = 0;
  #lastError?: string;
  #restApplied = false;
  #phoneFresh = false;
  #alarmFresh = false;
  #commandFresh = false;
  #contextBlocked = false;
  #contextComposition?: ReturnType<typeof contextComposition>;
  #modelBlocked: ModelBlock | null = null;
  #fatal = false;
  #sharedBudget?: {
    dailyTokenBudget: number;
    timeZone: string;
    paused: boolean;
    day: string;
    knownNoncachedTokens: number;
    unknownReservedTokens: number;
    safetyTotal: number;
    safeRemaining: number;
    model?: { blocked: ModelBlock | null };
  };
  constructor(
    readonly ctx: PluginContext,
    readonly config: EveConfig,
    readonly contextCounter?: ContextCounter,
  ) {
    this.journal = config.remoteRecords
      ? new RemoteJournal(ctx, config.directory)
      : new ConsciousJournal(config.directory);
  }
  async initialize() {
    this.#prompt = await Deno.readTextFile(this.config.promptFile);
    if (!this.#prompt.trim()) throw new KernelError("rejected", "需要稳定身份与真实能力说明");
    await this.journal.initialize();
    for (const event of activeEvents(this.journal.events)) {
      const data = event.data as Record<string, Json>;
      if (event.kind === "tool_result") {
        this.#rememberJob(String(data.callId), data.result);
      }
      if (event.kind === "notice") {
        if (
          ["image", "image_description"].includes(String(data.source)) &&
          typeof data.actionId === "string"
        ) this.#imageActions.add(data.actionId);
        if (data.source === "agents") this.#agentThrough = Number(data.throughSeq);
        if (data.source === "phone") this.#phoneThrough = Number(data.throughSeq);
        if (data.source === "scripts") this.#scriptThrough = Number(data.throughSeq);
        if (data.source === "commands") this.#commandThrough = Number(data.throughSeq);
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
        const failure = data.error as { kind: ErrorKind; message: string };
        const status = failure && providerRejection(new KernelError(failure.kind, failure.message));
        if (status !== undefined) {
          this.#modelBlocked = {
            category: "provider_rejected",
            httpStatus: status,
            requestId: String(data.requestId),
            at: event.at,
          };
        }
      }
      if (event.kind === "model_resume") this.#modelBlocked = null;
      if (event.kind === "model_response") {
        this.#failures = 0;
        this.#retryAt = 0;
        if ((data.response as unknown as ModelResponse).toolCalls.length) this.#wake = 0;
      }
    }
    const responded = new Set<string>();
    for (const event of activeEvents(this.journal.events)) {
      if (["model_response", "model_failure", "recovery"].includes(event.kind)) {
        responded.add((event.data as { requestId: string }).requestId);
      }
    }
    for (const event of activeEvents(this.journal.events)) {
      if (
        event.kind === "model_start" &&
        !responded.has((event.data as { requestId: string }).requestId)
      ) {
        if (this.config.remoteRecords) {
          try {
            const requestId = (event.data as { requestId: string }).requestId;
            const response = await this.ctx.call("model", "result", { requestId });
            await this.journal.append("model_response", { requestId, response, adopted: true });
            continue;
          } catch (error) {
            if (!(error instanceof KernelError && error.kind === "not_found")) throw error;
          }
        }
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
    if (this.#failures || pendingTools(this.journal.events).length) this.#wake = 0;
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
    this.#continueImmediately = false;
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
      this.#continueImmediately = false;
    } finally {
      this.#busy = undefined;
      this.#schedule(this.#continueImmediately ? 0 : this.config.pollMs);
    }
  }
  async #refreshBudget() {
    if (this.config.remoteRecords) {
      const shared = await this.ctx.call("model", "budget", {}) as unknown as {
        dailyTokenBudget: number;
        timeZone: string;
        paused: boolean;
        day: string;
        knownNoncachedTokens: number;
        unknownReservedTokens: number;
        safetyTotal: number;
        safeRemaining: number;
        model?: { blocked: ModelBlock | null };
      };
      const changed = canonical((this.#sharedBudget ?? null) as unknown as Json) !==
        canonical(shared as unknown as Json);
      this.#sharedBudget = shared;
      if (shared.model) this.#modelBlocked = shared.model.blocked;
      this.config.dailyTokenBudget = shared.dailyTokenBudget;
      this.config.timeZone = shared.timeZone;
      if (changed) {
        await this.ctx.call("mind", "update", { dailyBudget: shared.dailyTokenBudget, ...shared });
      }
    }
  }
  async #notices(): Promise<boolean> {
    this.#restApplied = false;
    this.#phoneFresh = false;
    this.#agentFresh = false;
    this.#alarmFresh = false;
    this.#commandFresh = false;
    let fresh = false;
    // Reads are independent; apply their receipts in the established deterministic order.
    const [, phone, tasks, completed, controls, agents] = await Promise.all([
      this.#refreshBudget(),
      this.ctx.call<Json>("phone", "notifications", { afterSeq: this.#phoneThrough }),
      this.ctx.call<Json>("scripts", "completions", { afterSeq: this.#scriptThrough }),
      this.config.remoteRecords
        ? this.ctx.call("commands", "completions", { afterSeq: this.#commandThrough })
        : Promise.resolve(undefined),
      this.ctx.call<Json>("mind", "poll", { afterSeq: this.#mindThrough }),
      this.config.agentsEnabled
        ? this.ctx.call<Json>("agents", "notifications", { afterSeq: this.#agentThrough })
        : Promise.resolve(undefined),
    ]);
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
    if (agents) {
      const page = agents as unknown as { items: Json[]; throughSeq: number };
      if (page.throughSeq > this.#agentThrough) {
        await this.journal.append("notice", {
          source: "agents",
          throughSeq: page.throughSeq,
          items: page.items,
        });
        this.#agentThrough = page.throughSeq;
        if (page.items.length) {
          fresh = true;
          this.#agentFresh = true;
          this.#wake = 0;
        }
      }
    }
    const completion = tasks as unknown as { items: Json[]; throughSeq: number };
    if (completion.throughSeq > this.#scriptThrough) {
      for (const item of completion.items) {
        const job = (item as { job?: { jobId?: string } }).job;
        if (job?.jobId) await this.#observations(job.jobId);
      }
      const newItems = completion.items.filter((item) => {
        const job = (item as { job?: { jobId?: string; status?: string } }).job;
        return !job?.jobId || this.#observedJobs.get(job.jobId)?.status !== job.status;
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
    if (this.config.remoteRecords) {
      const page = completed as unknown as { items: Json[]; throughSeq: number };
      if (page.throughSeq > this.#commandThrough) {
        await this.journal.append("notice", {
          source: "commands",
          throughSeq: page.throughSeq,
          items: page.items,
        });
        this.#commandThrough = page.throughSeq;
        if (page.items.length) {
          fresh = true;
          this.#commandFresh = true;
          this.#wake = 0;
        }
      }
    }
    const events = controls as unknown as { items: MindEvent[]; throughSeq: number };
    for (const event of events.items) {
      let wakeAt: number | undefined;
      let outcome: Json = { status: "applied" };
      if (["alarm", "condition_error", "condition_recovered"].includes(event.kind)) {
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
  async #observations(jobId: string) {
    const known = this.#imageActions;
    const callId = this.#observedJobs.get(jobId);
    for (const method of ["observations", "images"]) {
      let afterSeq = 0;
      do {
        const page = await this.ctx.call("scripts", method, { jobId, afterSeq }) as unknown as {
          items: { actionId: string; observation?: Json; image?: Json }[];
          throughSeq: number;
          hasMore: boolean;
        };
        for (const item of page.items) {
          if (!known.has(item.actionId)) {
            await this.journal.append("notice", {
              source: method === "images" ? "image" : "image_description",
              selectedMode: method === "images" ? "original" : "describe",
              jobId,
              ...(callId?.callId ? { callId: callId.callId } : {}),
              ...item,
            });
            known.add(item.actionId);
          }
        }
        if (!page.hasMore) break;
        if (page.throughSeq <= afterSeq) throw new KernelError("corrupt", "图像回执游标未推进");
        afterSeq = page.throughSeq;
      } while (true);
    }
  }

  async #tool(pending: PendingTool) {
    const call = pending.call;
    this.#toolStartedAt.set(call.id, performance.now());
    if (!pending.started) {
      await this.journal.append("tool_start", {
        responseSeq: pending.responseSeq,
        index: pending.index,
        callId: call.id,
        call: withoutThought(call) as unknown as Json,
      });
    }
    let result: Json;
    if (CONTEXT_TOOLS.some((tool) => tool.name === call.name)) {
      try {
        // 已落盘压缩但尚未写回执时，重启只补齐原收据，不重新执行或推测生效。
        const applied = activeEvents(this.journal.events).find((event) =>
          event.kind === "compression" && (event.data as { callId?: string }).callId === call.id
        );
        let output: Json;
        if (applied) {
          output = {
            ...(applied.data as Record<string, Json>).receipt as Record<string, Json>,
            compressionSeq: applied.seq,
          };
        } else {
          const tools = await this.#tools();
          if (call.name === "inspect_context") {
            output = inspectContext(
              this.journal.events,
              this.#prompt,
              tools,
              call.args,
              this.contextCounter,
              (messages) => this.#present(messages),
            ) as unknown as Json;
          } else {
            const preview = previewCompression(
              this.journal.events,
              this.#prompt,
              tools,
              call.args,
              this.contextCounter,
              (messages) => this.#present(messages),
            );
            const receipt = { status: preview.dryRun ? "preview" : "applied", ...preview };
            // 摘要原件只在压缩事实中保留，回执只报告边界和用量。
            const { summary: _summary, ...reported } = receipt;
            if (preview.dryRun) output = reported as unknown as Json;
            else {
              const event = await this.journal.append("compression", {
                throughSeq: preview.throughSeq,
                summary: preview.summary,
                source: "eve",
                callId: call.id,
                receipt: reported as unknown as Json,
              });
              output = { ...reported, compressionSeq: event.seq } as unknown as Json;
            }
          }
        }
        result = { status: "completed", output: JSON.stringify(output) };
      } catch (error) {
        if (!(error instanceof KernelError && error.kind === "rejected")) throw error;
        result = { status: "failed", error: toWireError(error) as unknown as Json };
      }
      await this.#saveToolResult(call.id, result);
      return;
    }
    if (call.name === WRITE_THOUGHT_TOOL.name) {
      const valid = thoughtForCall(call) !== undefined &&
        Object.keys(call.args as Record<string, Json>).every((key) => key === "thought");
      result = valid
        ? {
          status: "completed",
          output: JSON.stringify({
            recorded: true,
            responseSeq: pending.responseSeq,
            toolIndex: pending.index,
          }),
        }
        : { status: "failed", output: "需要非空的简短thought字符串，且不接受其他参数" };
      // 正文已经在对应 model_response 原件中。重启只补齐这条工具回执。
      await this.#saveToolResult(call.id, result);
      return;
    }
    if (pending.started && call.name === "execute_script") {
      const jobId = await fingerprint(["script-job-v1", call.id]);
      try {
        const summary = await this.ctx.call<Json>("scripts", "status", { jobId });
        result = { status: "completed", output: JSON.stringify(summary), adopted: true };
        await this.#saveToolResult(call.id, result);
        await this.#observations(jobId);
        return;
      } catch (error) {
        if (!(error instanceof KernelError && error.kind === "not_found")) throw error;
      }
    }
    if (
      call.name === "execute_script" && call.args && typeof call.args === "object" &&
      !Array.isArray(call.args) && "thought" in call.args && thoughtForCall(call) === undefined
    ) {
      // 不静默丢弃模型明确要求记录的内容，也不重写原始响应。
      // 若旧版本已启动同一任务，上面的恢复查询仍优先保留真实执行状态。
      await this.journal.append("tool_result", {
        callId: call.id,
        result: {
          status: "failed",
          output: "thought参数无效：需要不超过4000字符的非空字符串；本次未派发脚本",
        },
      });
      return;
    }
    try {
      result = await this.ctx.call<Json>(
        "tools",
        "execute",
        withoutThought(call) as unknown as Json,
        {
          timeoutMs: this.config.modelTimeoutMs,
        },
      );
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
    await this.#saveToolResult(call.id, result);
    if (call.name === "execute_script") {
      await this.#observations(await fingerprint(["script-job-v1", call.id]));
    }
  }
  async #projectContext(tools: ToolSpec[]) {
    const events = this.journal.events;
    const previous = lastActiveEvent(events, "context_projection");
    const messages = this.#render();
    const estimated = contextEstimate(messages, tools, this.contextCounter);
    // 固定 epoch 只在首次迁移或值得支付一次缓存失效时前移。retain/压缩
    // 自己的事实仍立即生效，但不因此每一轮滑动最近八批或记忆来源。
    if (previous && estimated < 120_000) return messages;
    const throughSeq = events.at(-1)?.seq ?? 0;
    if (previous && (previous.data as { throughSeq: number }).throughSeq === throughSeq) {
      return messages;
    }
    const preview: ConsciousEvent = {
      schema: 1,
      seq: throughSeq + 1,
      at: new Date().toISOString(),
      kind: "context_projection",
      data: { throughSeq },
      previous: events.at(-1)?.digest ?? "0".repeat(64),
      digest: "preview",
    };
    // 完整原件构造临时视图，不写回、追加或修改任何原事件。预演不使用
    // 已投影的 messages 作输入，避免把节选误当原件再压缩。
    const projected = this.#present(render([...events, preview], this.#prompt));
    const projectedTokens = contextEstimate(projected, tools, this.contextCounter);
    if (previous && estimated - projectedTokens < 8_000) return messages;
    // 单条持久事实先提交，随后重启/回滚都从活动日志重建同一边界。
    await this.journal.append("context_projection", {
      throughSeq,
      reason: previous ? "context_high_watermark" : "initial_fixed_projection",
      estimatedBefore: estimated,
      estimatedAfter: projectedTokens,
    });
    return this.#render();
  }
  async #tools(): Promise<ToolSpec[]> {
    const provided = await this.ctx.call<Json>("tools", "list") as unknown as ToolSpec[];
    const native = [WRITE_THOUGHT_TOOL, ...CONTEXT_TOOLS];
    if (provided.some((tool) => native.some((item) => item.name === tool.name))) {
      throw new KernelError("rejected", "意识流原生工具不能由外部覆盖");
    }
    return [...provided, ...native];
  }
  async #step() {
    if (this.#closing || !this.journal.available) return;
    // 每次工具批次先完成/恢复原决定，再处理通知；新的通知不打断当前动作。
    const preparationStarted = performance.now();
    const pending = pendingTools(this.journal.events);
    const pendingScanMs = performance.now() - preparationStarted;
    if (pending.length) {
      for (const tool of pending) {
        if (this.#closing) return;
        await this.#tool(tool);
      }
      if (!this.#closing) this.#continueImmediately = true;
      return;
    }
    const noticesStarted = performance.now();
    const fresh = await this.#notices();
    const noticesMs = performance.now() - noticesStarted;
    if (
      this.#restApplied && !this.#phoneFresh && !this.#alarmFresh && !this.#commandFresh &&
      !this.#agentFresh
    ) return;
    if (fresh) {
      this.#wake = 0;
      await this.journal.append("wake", {
        reason: "新的未观察通知",
        sourceCommit: this.config.sourceCommit,
      });
    }
    if (this.#closing) return;
    if (this.#modelBlocked) return;
    if (Date.now() < this.#retryAt) return;
    if (!fresh && Date.now() < this.#wake) return;
    const toolsStarted = performance.now();
    const tools = await this.#tools();
    const toolsListMs = performance.now() - toolsStarted;
    const recallStarted = performance.now();
    if (this.config.remoteRecords) {
      if (this.#sharedBudget?.paused) return;
      const query = recallQuery(activeEvents(this.journal.events));
      const anchor = activeEvents(this.journal.events).findLast((e) =>
        e.kind === "model_response" || e.kind === "tool_result" ||
        e.kind === "notice" && (e.data as { source?: string }).source !== "memory"
      )?.seq ?? 0;
      const lastRecall = activeEvents(this.journal.events).findLast((e) =>
        e.kind === "notice" && (e.data as { source?: string }).source === "memory"
      );
      if (query && (lastRecall?.data as { anchor?: number } | undefined)?.anchor !== anchor) {
        let recalled: { items: Json[] };
        try {
          recalled = await this.ctx.call("memory", "recall", {
            query,
            limit: 3,
            _actionId: `recall_${anchor}`,
          }) as {
            items: Json[];
          };
        } catch (error) {
          if (
            !(error instanceof KernelError && ["unavailable", "not_found"].includes(error.kind))
          ) {
            throw error;
          }
          recalled = { items: [] };
        }
        await this.journal.append("notice", { source: "memory", anchor, items: recalled.items });
      }
    }
    const recallMs = performance.now() - recallStarted;
    const lastResponse = activeEvents(this.journal.events).findLast((e) =>
      e.kind === "model_response"
    );
    const hasActions = lastResponse &&
      (lastResponse.data as unknown as { response: ModelResponse }).response.toolCalls.length > 0;
    const pendingJobs = [...this.#observedJobs.values()].some((j) =>
      !["completed", "failed", "cancelled", "unknown"].includes(j.status)
    );
    const selfContextStarted = performance.now();
    await this.#selfContext(!fresh && !hasActions && !pendingJobs);
    const selfContextMs = performance.now() - selfContextStarted;
    const contextStarted = performance.now();
    // 必须先固定初次投影，再判断 500k 机械保护，避免旧 raw 历史误触压缩。
    let messages = await this.#projectContext(tools);
    const compression = contextEstimate(messages, tools, this.contextCounter) >= 500_000
      ? mechanicalCompression(this.journal.events, this.#prompt, tools, this.contextCounter)
      : undefined;
    if (compression) {
      await this.journal.append("compression", { ...compression, source: "system" });
      messages = await this.#projectContext(tools);
    }
    const contextProjectionMs = performance.now() - contextStarted;
    const estimated = contextEstimate(messages, tools, this.contextCounter);
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
    const day = this.#sharedBudget?.day ?? accountingDay(this.config.timeZone);
    const usage = this.#sharedBudget ?? budget(this.journal.events, day);
    let reservation = new TextEncoder().encode(JSON.stringify({ messages, tools })).length +
      messages.length * 32 + this.config.maxOutputTokens;
    const state = {
      at: new Date().toISOString(),
      paused: false,
      contextCounting: this.contextCounter?.description ??
        "字符与图像估算，未验证为当前模型精确分词",
      sourceCommit: this.config.sourceCommit,
      contextEstimatedTokens: estimated,
      contextTargetTokens: 150_000,
      contextTargetCounting: "上游实际inputTokens平均目标；本地组成数字仅为估算",
      contextComposition: contextComposition(messages),
      contextReminder: estimated >= 120_000
        ? "上下文输入目标低于150k；已达到120k估算提醒线，可自主整理时间线、摘要或工具结果，原件保留"
        : null,
      dailyBudget: this.config.dailyTokenBudget,
      knownNoncachedTokens: usage.knownNoncachedTokens,
      unknownReservedTokens: usage.unknownReservedTokens,
      safeRemaining: Math.max(0, this.config.dailyTokenBudget - usage.safetyTotal),
      pendingTools: 0,
    };
    const stateStarted = performance.now();
    await this.ctx.call("mind", "update", state as unknown as Json);
    const stateUpdateMs = performance.now() - stateStarted;
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
    const last = lastActiveEvent(this.journal.events, "perception");
    if (
      !last || (last.data as { contextBand: number }).contextBand !== band ||
      (last.data as { day: string }).day !== day ||
      (last.data as { remainingBand: number }).remainingBand !==
        Math.floor(state.safeRemaining / 10_000_000) ||
      (last.data as { dailyBudget?: number }).dailyBudget !== this.config.dailyTokenBudget ||
      (last.data as { reminder?: string | null }).reminder !== state.contextReminder
    ) {
      await this.journal.append("perception", {
        contextBand: band,
        day,
        sourceCommit: this.config.sourceCommit,
        remainingBand: Math.floor(state.safeRemaining / 10_000_000),
        dailyBudget: this.config.dailyTokenBudget,
        counting: this.contextCounter?.description ?? "字符与图像估算，未验证为当前模型精确分词",
        reminder: state.contextReminder,
      });
      messages = this.#render();
    }
    // 感知本身也属于请求；不能在刚低于触线时追加后直接发出超线请求。
    if (contextEstimate(messages, tools, this.contextCounter) >= 500_000) {
      const finalCompression = mechanicalCompression(
        this.journal.events,
        this.#prompt,
        tools,
        this.contextCounter,
      );
      if (finalCompression) {
        await this.journal.append("compression", { ...finalCompression, source: "system" });
        messages = await this.#projectContext(tools);
      }
      if (contextEstimate(messages, tools, this.contextCounter) >= 500_000) {
        this.#contextBlocked = true;
        await this.journal.append("context_wait", { reason: "最终完整请求仍超出估算触线" });
        return;
      }
    }
    reservation = new TextEncoder().encode(JSON.stringify({ messages, tools })).length +
      messages.length * 32 + this.config.maxOutputTokens;
    this.#contextComposition = contextComposition(messages);
    if (usage.safetyTotal + reservation > this.config.dailyTokenBudget) {
      await this.journal.append("budget_wait", { day, reason: "含感知的完整请求预算余量不足" });
      return;
    }
    if (this.#closing) return;
    if (this.#selfIdeas.length) {
      await this.journal.append("self_reflection", { ideas: this.#selfIdeas } as unknown as Json);
    }
    const requestId = `model_${this.journal.events.length + 1}`;
    await this.journal.append("model_start", {
      requestId,
      day,
      reservation,
      estimatedTokens: contextEstimate(messages, tools, this.contextCounter),
      contextComposition: this.#contextComposition,
      sourceCommit: this.config.sourceCommit,
      runtimeTiming: {
        pendingScanMs,
        noticesMs,
        toolsListMs,
        recallMs,
        selfContextMs,
        contextProjectionMs,
        stateUpdateMs,
        contextAndStateMs: performance.now() - contextStarted,
        preparationMs: performance.now() - preparationStarted,
      },
    });
    let response: ModelResponse;
    try {
      if (this.#closing) throw new KernelError("unavailable", "实例已进入停止，模型请求未派发");
      response = await this.ctx.call<Json>(
        "model",
        "complete",
        {
          messages,
          tools,
          ...(this.config.remoteRecords ? { _actionId: requestId } : {}),
        } as unknown as Json,
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
      const rejectedStatus = providerRejection(error);
      if (rejectedStatus !== undefined) {
        this.#modelBlocked = {
          category: "provider_rejected",
          httpStatus: rejectedStatus,
          requestId,
          at: new Date().toISOString(),
        };
      }
      this.#retryAt = rejectedStatus === undefined
        ? Date.now() + Math.min(300_000, 1000 * 2 ** Math.min(this.#failures, 18))
        : 0;
      await this.journal.append("model_failure", {
        requestId,
        error: toWireError(error) as unknown as Json,
        retryAt: this.#retryAt,
        ...(this.#modelBlocked ? { blocked: this.#modelBlocked as unknown as Json } : {}),
      });
      return;
    }
    await this.journal.append("model_response", {
      requestId,
      sourceCommit: this.config.sourceCommit,
      response: response as unknown as Json,
    });
    // Shared accounting is already durable; refresh it on the next activation.
    // Preserve the model receipt before any optional read during shutdown.
    this.#failures = 0;
    this.#retryAt = 0;
    if (response.toolCalls.length === 0) {
      this.#wake = Date.now() + this.config.defaultSleepMs;
      await this.journal.append("rest", {
        wakeAt: this.#wake,
        sourceCommit: this.config.sourceCommit,
        reason: "本步没有继续动作；默认长唤醒，Eve可改",
      });
    } else {
      this.#wake = 0;
      this.#continueImmediately = true;
    }
  }
  status() {
    return {
      running: this.#running,
      busy: this.#busy !== undefined,
      healthy: this.journal.available && !this.#closing && !this.#fatal,
      sourceCommit: this.config.sourceCommit,
      records: this.journal.events.length,
      agentThroughSeq: this.#agentThrough,
      phoneThroughSeq: this.#phoneThrough,
      scriptThroughSeq: this.#scriptThrough,
      commandThroughSeq: this.#commandThrough,
      mindThroughSeq: this.#mindThrough,
      wakeAt: this.#wake,
      budget: this.#sharedBudget ??
        budget(this.journal.events, accountingDay(this.config.timeZone)),
      lastError: this.#lastError ?? null,
      contextBlocked: this.#contextBlocked,
      contextTargetTokens: 150_000,
      contextTargetCounting: "上游实际inputTokens平均目标；本地组成数字仅为估算",
      contextComposition: this.#contextComposition ?? null,
      modelBlocked: this.#modelBlocked,
    };
  }
  async resumeModel(expectedRequestId: string) {
    if (this.config.remoteRecords) {
      throw new KernelError("rejected", "独立运行模式的模型阻塞只能由管理出口显式恢复");
    }
    await this.#busy;
    if (!this.#modelBlocked || this.#modelBlocked.requestId !== expectedRequestId) {
      throw new KernelError("rejected", "模型阻塞状态已变化，请刷新状态后显式恢复");
    }
    await this.journal.append("model_resume", {
      requestId: expectedRequestId,
      reason: "operator_explicit_resume",
    });
    this.#modelBlocked = null;
    this.#retryAt = 0;
    return this.status();
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
