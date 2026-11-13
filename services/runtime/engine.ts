import { AgentService } from "./agents.ts";
import type {
  ModelCallOptions,
  ModelImage,
  ModelRequest,
  ModelResponse,
} from "../../contracts/model.ts";
import { RUNTIME_METHODS } from "../../contracts/runtime.ts";
import { canonical, fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError, toWireError, type WireError } from "../../kernel/protocol.ts";
import { WebService } from "../web.ts";
import type { WebApi } from "../../contracts/web.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { type CommandIdentity, CommandSessions } from "./commands.ts";
import { SharedBudget } from "./budget.ts";
import { FileTools } from "./files.ts";
import { type FileRuntime, RestrictedFileTools } from "./restricted-files.ts";
import { NoteStore, textPage } from "./notes.ts";
import { MemoryStore } from "./memory.ts";
import type { LocalSemantic } from "./embedding.ts";
import { describedImage, imageDescriptionRequest, imageProvenance } from "./vision.ts";
import type { ModelSelection, ModelSelectionResult, ModelSnapshot } from "./model-selection.ts";
import { type ModelBlock, providerRejection } from "../../sdk/model-failure.ts";

interface RequestRecord {
  id: string;
  hash: string;
  namespace: string;
  method: string;
  recovery: Json;
  value?: Json;
  error?: WireError;
  terminal: boolean;
}
interface Options {
  directory: string;
  workspace: string;
  allowedRoots?: string[];
  settingsFile: string;
  model: {
    complete(args: ModelRequest, options?: ModelCallOptions): Promise<ModelResponse>;
    close(): void;
  };
  maxOutputTokens: number;
  modelName?: string;
  modelSelection?: Pick<ModelSelection, "snapshot" | "current">;
  commandIdentity?: CommandIdentity;
  commandUseSystemd?: boolean;
  fileRuntime?: FileRuntime;
  semantic?: Pick<LocalSemantic, "embed" | "count">;
  web?: WebApi;
}
/** 真实执行与回执不随内核/插件回退；令牌校验由独立 HTTP 出口包围整个执行。 */
export class RuntimeEngine {
  readonly agents: AgentService;
  readonly web: WebApi;
  readonly records: FactJournal;
  readonly transactions: FactJournal;
  readonly modelResponses: FactJournal;
  readonly notes: NoteStore;
  readonly memory: MemoryStore;
  readonly files: FileTools;
  readonly commands: CommandSessions;
  readonly budget: SharedBudget;
  #requests = new Map<string, RequestRecord>();
  #responseIndex = new Map<string, number>();
  #live = new Map<string, Promise<Json>>();
  #timelineTail: Promise<unknown> = Promise.resolve();
  #modelSelectionTail: Promise<unknown> = Promise.resolve();
  #closing = false;
  #modelBlocked: (ModelBlock & { modelName?: string; modelRevision?: number }) | null = null;
  constructor(readonly options: Options) {
    const directory = options.directory;
    this.web = options.web ?? new WebService();
    const embed = options.semantic
      ? (text: string, purpose?: "query" | "passage") => options.semantic!.embed(text, purpose)
      : undefined;
    this.records = new FactJournal(`${directory}/consciousness`);
    this.transactions = new FactJournal(`${directory}/actions`);
    this.modelResponses = new FactJournal(`${directory}/model-responses`);
    this.notes = new NoteStore(
      `${directory}/notes`,
      embed,
      this.records,
      options.semantic ? (text) => options.semantic!.count(text) : undefined,
    );
    this.memory = new MemoryStore(`${directory}/memory`, this.records, embed);
    this.files = options.commandIdentity
      ? new RestrictedFileTools(
        options.workspace,
        options.allowedRoots,
        options.commandIdentity,
        options.fileRuntime,
      )
      : new FileTools(options.workspace, options.allowedRoots);
    this.commands = new CommandSessions(
      `${directory}/commands`,
      this.files,
      options.commandIdentity,
      options.commandUseSystemd,
    );
    this.budget = new SharedBudget(`${directory}/budget`, options.settingsFile);
    this.agents = new AgentService(`${directory}/agents`, {
      result: (id) => this.#modelResult(id) as unknown as ModelResponse | undefined,
      complete: async (request, config, id) =>
        await this.dispatch(
          "model",
          "complete",
          { ...request, _purpose: "isolated", _agentConfig: config } as unknown as Record<
            string,
            Json
          >,
          id,
        ) as unknown as ModelResponse,
      invoke: (namespace, method, args, id) => this.dispatch(namespace, method, args, id),
    });
  }
  get available() {
    return !this.#closing && this.transactions.available && this.records.available &&
      this.notes.journal.available && this.memory.journal.available &&
      this.budget.journal.available && this.agents.available;
  }
  async initialize() {
    await Deno.mkdir(this.options.workspace, { recursive: true, mode: 0o700 });
    try {
      for (
        const service of [
          this.records,
          this.transactions,
          this.modelResponses,
          this.notes,
          this.memory,
          this.commands,
          this.budget,
        ]
      ) await service.initialize();
      const responseIds = new Set<string>();
      for (const event of this.modelResponses.events) {
        const data = event.data as unknown as { id: string; response: ModelResponse };
        if (
          event.kind !== "response" || !data || typeof data.id !== "string" ||
          responseIds.has(data.id) || !data.response || typeof data.response.content !== "string" ||
          !Array.isArray(data.response.toolCalls)
        ) {
          throw new KernelError("corrupt", "模型响应原件索引无效，保留现场");
        }
        responseIds.add(data.id);
        this.#responseIndex.set(data.id, event.seq);
        // 原响应已落盘而预算结算前中断时，用原用量补记；不再调用模型或重复强化记忆。
        await this.budget.response(data.id, data.response);
      }
      for (const event of this.transactions.events) {
        const data = event.data as unknown as RequestRecord;
        if (event.kind === "request") {
          if (
            !data || typeof data.id !== "string" || !/^[a-f0-9]{64}$/.test(data.hash) ||
            this.#requests.has(data.id)
          ) throw new KernelError("corrupt", "执行请求索引异常");
          this.#requests.set(data.id, { ...data, terminal: false });
        } else if (event.kind === "result") {
          const previous = this.#requests.get(data.id);
          if (
            !previous || previous.terminal ||
            data.error === undefined && !Object.hasOwn(data, "value")
          ) throw new KernelError("corrupt", "执行回执缺少原请求");
          Object.assign(previous, data, { terminal: true });
        } else throw new KernelError("corrupt", "未知执行记录");
      }
      // Reconcile agent originals first, so a completed reply also restores its outer receipt.
      await this.agents.initialize();
      for (const r of this.#requests.values()) {
        if (!r.terminal) {
          let value: Json | undefined;
          if (r.namespace === "agents" && r.method === "send") {
            try {
              value = this.agents.status({ jobId: r.id }) as unknown as Json;
            } catch (error) {
              if (!(error instanceof KernelError && error.kind === "not_found")) throw error;
            }
          }
          if (r.namespace === "notes") {
            value = this.notes.receipt(r.id) as unknown as Json | undefined;
          }
          if (r.namespace === "memory" && r.method === "remember") {
            value = this.memory.receipt(r.id) as unknown as Json | undefined;
          }
          if (r.namespace === "model" && r.method === "complete") value = this.#modelResult(r.id);
          if (r.namespace === "model" && r.method === "describeImage") {
            const response = this.#modelResult(r.id);
            if (response !== undefined) {
              value = describedImage(
                response as unknown as ModelResponse,
                (r.recovery as unknown as { provenance: ModelImage["provenance"] }).provenance,
                (r.recovery as { modelName?: string }).modelName,
              ) as unknown as Json;
            }
          }
          if (r.namespace === "timeline" && r.method === "append") {
            const recovery = r.recovery as { expectedSeq: number; digest: string };
            const event = this.records.events[recovery.expectedSeq];
            if (event && await fingerprint([event.kind, event.data]) === recovery.digest) {
              value = event as unknown as Json;
            }
          }
          await this.#result(
            r,
            value === undefined
              ? { error: { kind: "unknown", message: "宿主中断时结果未确认，原请求不重放" } }
              : { value },
          );
        }
      }
      // Resume is a separate, authenticated fact. Pausing, new notifications and restarts
      // never erase a permanent rejection of the online consciousness request.
      const resumed = this.records.events.findLast((event) => event.kind === "model_resume");
      const after = Number(
        (resumed?.data as { actionsThroughSeq?: number } | undefined)?.actionsThroughSeq ?? 0,
      );
      if (!Number.isSafeInteger(after) || after < 0 || after > this.transactions.events.length) {
        throw new KernelError("corrupt", "模型恢复事实水位无效，保留原件");
      }
      for (let index = after; index < this.transactions.events.length; index++) {
        const event = this.transactions.events[index];
        const data = event.data as unknown as RequestRecord;
        const request = this.#requests.get(data.id);
        if (
          event.kind !== "result" || request?.namespace !== "model" ||
          request.method !== "complete" || !/^model_[1-9][0-9]*$/.test(data.id)
        ) continue;
        const status = data.error
          ? providerRejection(new KernelError(data.error.kind, data.error.message))
          : undefined;
        if (status !== undefined) {
          this.#blockModel(data.id, status, event.at, request.recovery);
        }
      }
    } catch (error) {
      await this.close().catch(() => {});
      throw error;
    }
  }
  modelStatus() {
    return {
      blocked: this.#modelBlocked ? structuredClone(this.#modelBlocked) : null,
      ...(this.options.modelSelection ? { current: this.options.modelSelection.current() } : {}),
    };
  }
  #blockModel(
    requestId: string,
    httpStatus: number,
    at = new Date().toISOString(),
    recovery?: Json,
  ) {
    const captured = recovery as { modelName?: string; modelRevision?: number } | undefined;
    const modelName = captured?.modelName ?? this.options.modelName;
    const modelRevision = captured?.modelRevision ?? 0;
    const current = this.options.modelSelection?.current();
    // A late permanent rejection from a previous selection cannot stop the new model.
    if (current && (current.model !== modelName || current.revision !== modelRevision)) return;
    if (
      this.#modelBlocked && (!current ||
        this.#modelBlocked.modelName === modelName &&
          this.#modelBlocked.modelRevision === modelRevision)
    ) return;
    const blocked: ModelBlock & { modelName?: string; modelRevision?: number } = {
      category: "provider_rejected",
      httpStatus,
      requestId,
      at,
      ...(this.options.modelSelection ? { modelName, modelRevision } : {}),
    };
    this.#modelBlocked = blocked;
  }
  async modelSelectionChanged(result: ModelSelectionResult) {
    const work = this.#modelSelectionTail.then(async () => {
      const current = this.options.modelSelection?.current(), blocked = this.#modelBlocked;
      if (
        !current || !result.changed || result.model !== current.model ||
        result.revision !== current.revision || !blocked ||
        blocked.modelName === current.model && blocked.modelRevision === current.revision
      ) return;
      // Receipts may be queried concurrently or after a lost HTTP response. Apply exactly once.
      const applied = this.records.events.some((event) => {
        const data = event.data as { reason?: string; actionId?: string; requestId?: string };
        return event.kind === "model_resume" && data.reason === "owner_explicit_model_switch" &&
          data.actionId === result.actionId && data.requestId === blocked.requestId;
      });
      if (!applied) {
        await this.records.append("model_resume", {
          requestId: blocked.requestId,
          reason: "owner_explicit_model_switch",
          actionId: result.actionId,
          previousModel: blocked.modelName ?? this.options.modelName ?? "",
          previousRevision: blocked.modelRevision ?? 0,
          model: result.model,
          modelRevision: result.revision,
          actionsThroughSeq: this.transactions.events.length,
        });
      }
      if (this.#modelBlocked === blocked) this.#modelBlocked = null;
    });
    this.#modelSelectionTail = work.catch(() => {});
    await work;
  }
  async resumeModel(expectedRequestId: string) {
    if (!this.#modelBlocked || this.#modelBlocked.requestId !== expectedRequestId) {
      throw new KernelError("rejected", "模型阻塞状态已变化，请刷新状态后显式恢复");
    }
    if (!(await this.budget.status()).paused) {
      throw new KernelError("rejected", "请先暂停运行，再明确恢复被拒绝的模型请求");
    }
    await this.records.append("model_resume", {
      requestId: expectedRequestId,
      reason: "operator_explicit_resume",
      actionsThroughSeq: this.transactions.events.length,
    });
    this.#modelBlocked = null;
    return this.modelStatus();
  }
  async #result(record: RequestRecord, result: { value?: Json; error?: WireError }) {
    await this.transactions.append("result", { id: record.id, ...result } as unknown as Json);
    Object.assign(record, result, { terminal: true });
  }
  #value(record: RequestRecord): Json {
    if (!record.terminal) throw new KernelError("unknown", "原动作正在执行或尚未核对");
    if (record.error) throw new KernelError(record.error.kind, record.error.message);
    return structuredClone(record.value!);
  }
  receipt(id: string) {
    const record = this.#requests.get(id);
    return record?.terminal ? structuredClone(record) : undefined;
  }
  async validateRepeat(namespace: string, method: string, args: Record<string, Json>, id: string) {
    const old = this.#requests.get(id);
    if (old && old.hash !== await fingerprint({ namespace, method, args })) {
      throw new KernelError("rejected", "同一动作编号不能改变参数");
    }
  }
  async dispatch(
    namespace: string,
    method: string,
    args: Record<string, Json>,
    id: string,
  ): Promise<Json> {
    if (
      namespace === "model" && method === "complete" &&
      args._purpose !== undefined && args._purpose !== "online" && args._purpose !== "isolated"
    ) {
      throw new KernelError("rejected", "模型调用目的必须为online或isolated");
    }
    if (!this.available) throw new KernelError("unavailable", "执行服务不可用");
    if (
      !(namespace in RUNTIME_METHODS) ||
      !(RUNTIME_METHODS[namespace as keyof typeof RUNTIME_METHODS] as readonly string[]).includes(
        method,
      )
    ) throw new KernelError("rejected", "该能力不在契约内");
    // Read-only agent views use the metadata index and do not append an execution ledger
    // on every idle notification poll or duplicate a complete long body into a capped fact.
    if (
      namespace === "agents" &&
      ["list", "status", "history", "message", "search", "notifications"].includes(method)
    ) {
      return await this.agents.invoke(method, args, id);
    }
    if (namespace === "notes" && method === "self") {
      return this.notes.self(args as never) as unknown as Json;
    }
    // Capture before the first await: an admitted request keeps its original client and provenance.
    const selected = namespace === "model" && ["complete", "describeImage"].includes(method)
      ? this.options.modelSelection?.snapshot() ?? {
        client: this.options.model,
        modelName: this.options.modelName ?? "",
        maxOutputTokens: this.options.maxOutputTokens,
        revision: 0,
      }
      : undefined;
    const captured = structuredClone(args),
      hash = await fingerprint({ namespace, method, args: captured });
    const recovery = (namespace === "timeline" && method === "append"
      ? {
        expectedSeq: captured.expectedSeq,
        digest: await fingerprint([captured.kind, captured.data]),
      }
      : namespace === "model" && method === "describeImage"
      ? {
        provenance: imageProvenance((captured.image as unknown as ModelImage)?.provenance),
        ...(selected?.modelName
          ? { modelName: selected.modelName, modelRevision: selected.revision }
          : {}),
      }
      : namespace === "model" && method === "complete" && selected
      ? { modelName: selected.modelName, modelRevision: selected.revision }
      : null) as unknown as Json;
    const old = this.#requests.get(id);
    if (old && old.hash !== hash) throw new KernelError("rejected", "同一动作编号不能改变参数");
    if (old) return this.#live.has(id) ? await this.#live.get(id)! : this.#value(old);
    const operation = (async () => {
      const record: RequestRecord = { id, namespace, method, hash, recovery, terminal: false };
      this.#requests.set(id, record);
      await this.transactions.append("request", record as unknown as Json);
      try {
        const value = await this.#invoke(namespace, method, captured, id, selected);
        canonical(value);
        await this.#result(record, { value });
        return value;
      } catch (error) {
        if (!(error instanceof KernelError && ["storage", "corrupt"].includes(error.kind))) {
          await this.#result(record, { error: toWireError(error) });
          const status = providerRejection(error);
          if (
            namespace === "model" && method === "complete" &&
            /^model_[1-9][0-9]*$/.test(id) && status !== undefined
          ) {
            this.#blockModel(id, status, undefined, recovery);
          }
        }
        throw error;
      }
    })();
    this.#live.set(id, operation);
    try {
      return await operation;
    } finally {
      this.#live.delete(id);
    }
  }
  #modelResult(id: string): Json | undefined {
    const seq = this.#responseIndex.get(id);
    return ((seq === undefined ? undefined : this.modelResponses.events[seq - 1])?.data as {
      response?: Json;
    } | undefined)?.response;
  }
  async #invoke(
    namespace: string,
    method: string,
    args: Record<string, Json>,
    id: string,
    selected?: ModelSnapshot,
  ): Promise<Json> {
    if (namespace === "agents") return await this.agents.invoke(method, args, id);
    if (namespace === "timeline") {
      if (method === "page") return this.#recordPage(args, true) as unknown as Json;
      if (args.kind === "upgrade_rollback" || args.kind === "model_resume") {
        throw new KernelError("rejected", "监督与模型恢复事实只能由管理出口写入，实例不能伪造");
      }
      const operation = this.#timelineTail.then(async () => {
        if (
          args.expectedSeq !== this.records.events.length ||
          args.expectedPrevious !== (this.records.events.at(-1)?.digest ?? "0".repeat(64)) ||
          typeof args.kind !== "string" || !args.kind || args.kind.length > 128
        ) throw new KernelError("rejected", "意识流前件已变化，拒绝旧视图写入");
        return await this.records.append(args.kind, args.data);
      });
      this.#timelineTail = operation.catch(() => {});
      return await operation as unknown as Json;
    }
    if (namespace === "records") {
      if (method === "page") return this.#recordPage(args, false) as unknown as Json;
      if (!Number.isSafeInteger(args.seq) || Number(args.seq) < 1) {
        throw new KernelError("rejected", "需要原记录编号");
      }
      const event = this.records.events[Number(args.seq) - 1];
      if (!event) throw new KernelError("not_found", "没有该原记录");
      return {
        ...textPage(JSON.stringify(event), args.startChar as number, args.limit as number),
        seq: event.seq,
        digest: event.digest,
      };
    }
    if (namespace === "notes") {
      const value = method === "create" || method === "update"
        ? await this.notes.change(method, args, id)
        : await this.notes[method as "get" | "list" | "versions" | "search" | "self"](
          args as never,
        );
      if (method === "create" || method === "update") {
        await this.memory.observe(JSON.stringify(args));
      }
      return value as unknown as Json;
    }
    if (namespace === "web") {
      return await this.web[method as "search" | "fetch"](args as never) as unknown as Json;
    }
    if (namespace === "files") {
      return await this.files[method as "read" | "write" | "replace" | "list" | "search"](
        args as never,
      ) as unknown as Json;
    }
    if (namespace === "commands") {
      if (method === "start") {
        const started = await this.commands.start(args as never, id);
        // 对调用方可以先返回会话，出口许可始终持到真实进程和输出保存结束。
        await started.done;
        return await this.commands.poll({ sessionId: started.sessionId }) as unknown as Json;
      }
      return await this.commands[method as "poll" | "write" | "cancel" | "completions"](
        args as never,
      ) as unknown as Json;
    }
    if (namespace === "memory") {
      return (method === "remember"
        ? await this.memory.remember(args as never, id)
        : await this.memory[method as "recall" | "used" | "list" | "get"](
          args as never,
        )) as unknown as Json;
    }
    if (namespace === "model") {
      if (method === "budget") {
        return { ...await this.budget.status(), model: this.modelStatus() } as unknown as Json;
      }
      if (method === "result") {
        const result = this.#modelResult(String(args.requestId));
        if (result === undefined) throw new KernelError("not_found", "没有该模型请求回执");
        return result;
      }
      const description = method === "describeImage";
      const blocked = this.#modelBlocked;
      const appliesToSelected = blocked && (!this.options.modelSelection ||
        selected?.modelName === blocked.modelName && selected?.revision === blocked.modelRevision);
      if (method === "complete" && /^model_[1-9][0-9]*$/.test(id) && appliesToSelected) {
        throw new KernelError(
          "rejected",
          `模型请求未完成：provider_rejected (${blocked.httpStatus})`,
        );
      }
      const image = description ? args.image as unknown as ModelImage : undefined;
      const { _purpose: purpose, _agentConfig: agentConfig, ...modelArgs } = args;
      const overrides = agentConfig as unknown as ModelCallOptions | undefined;
      if (overrides && (!id.startsWith("script_") && !id.startsWith("agent_"))) {
        throw new KernelError("rejected", "子代理模型配置需要独立任务编号");
      }
      const outputTokens = overrides?.maxOutputTokens ?? selected?.maxOutputTokens ??
        this.options.maxOutputTokens;
      const request = description
        ? imageDescriptionRequest(image!)
        : modelArgs as unknown as ModelRequest;
      const amount = new TextEncoder().encode(JSON.stringify(request)).length +
        request.messages.length * 64 + outputTokens;
      await this.budget.reserve(id, amount);
      const response = await (selected?.client ?? this.options.model).complete(request, overrides);
      const savedResponse = await this.modelResponses.append("response", {
        id,
        response,
        ...(selected?.modelName || overrides?.model
          ? {
            modelName: overrides?.model ?? selected?.modelName,
            modelRevision: selected?.revision ?? 0,
          }
          : {}),
        ...(overrides
          ? { agentSession: overrides.sessionId, reasoningEffort: overrides.reasoningEffort }
          : {}),
      } as unknown as Json);
      this.#responseIndex.set(id, savedResponse.seq);
      await this.budget.response(id, response);
      if (description) {
        return describedImage(
          response,
          image!.provenance,
          selected?.modelName || this.options.modelName,
        ) as unknown as Json;
      }
      if (purpose === "online") {
        await this.memory.observe(response.content);
      }
      return response as unknown as Json;
    }
    throw new KernelError("rejected", "未知出口");
  }
  #recordPage(args: Record<string, Json>, full: boolean) {
    const after = args.afterSeq ?? 0;
    if (
      !Number.isSafeInteger(after) || Number(after) < 0 ||
      Number(after) > this.records.events.length
    ) throw new KernelError("rejected", "原记录游标无效");
    const page = [];
    let bytes = 0;
    for (
      let index = Number(after);
      index < this.records.events.length && page.length < (full ? 128 : 8);
      index++
    ) {
      const event = this.records.events[index];
      const size = full ? new TextEncoder().encode(JSON.stringify(event)).length : 0;
      if (page.length && bytes + size > 12_000_000) break;
      page.push(event);
      bytes += size;
    }
    return {
      items: full
        ? structuredClone(page)
        : page.map(({ seq, at, kind, digest }) => ({ seq, at, kind, digest })),
      throughSeq: page.at(-1)?.seq ?? after,
      hasMore: Number(after) + page.length < this.records.events.length,
    };
  }
  async close() {
    this.#closing = true;
    const errors = [];
    try {
      await this.commands.close();
    } catch (e) {
      errors.push(e);
    }
    await this.agents.close();
    await Promise.allSettled([...this.#live.values(), this.#modelSelectionTail]);
    for (
      const service of [
        this.budget,
        this.memory,
        this.notes,
        this.modelResponses,
        this.transactions,
        this.records,
        this.files,
      ]
    ) {
      try {
        await service.close();
      } catch (e) {
        errors.push(e);
      }
    }
    this.options.model.close();
    if (errors.length) throw new KernelError("storage", "执行服务停止有失败，原件保留");
  }
}
