import type { ModelCallOptions, ModelRequest, ModelResponse } from "../../contracts/model.ts";
import { type Json, KernelError, type WireError } from "../../kernel/protocol.ts";
import { ChatModelClient, type ChatModelOptions } from "../../plugins/model-chat/client.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";

export interface ModelSnapshot {
  modelName: string;
  revision: number;
  maxOutputTokens: number;
  client: {
    complete(args: ModelRequest, options?: ModelCallOptions): Promise<ModelResponse>;
    close(): void;
  };
}
export interface ModelSelectionResult {
  actionId: string;
  model: string;
  revision: number;
  changed: boolean;
}
interface SelectionRequest {
  actionId: string;
  model: string;
  result?: ModelSelectionResult;
  error?: WireError;
}
export function modelCommandId(value: unknown): value is string {
  return typeof value === "string" && /^qq_model_[a-f0-9]{64}$/.test(value);
}
function modelId(value: unknown): value is string {
  return typeof value === "string" && /^[a-zA-Z0-9][a-zA-Z0-9._:/@+-]{0,255}$/.test(value);
}

/** A committed result is both the selection and the receipt; no second settings write can diverge. */
export class ModelSelection {
  readonly journal: FactJournal;
  #current: ModelSnapshot;
  #clients = new Map<string, ModelSnapshot["client"]>();
  #requests = new Map<string, SelectionRequest>();
  #tail: Promise<unknown> = Promise.resolve();
  #closed = new AbortController();
  constructor(readonly directory: string, readonly config: ChatModelOptions) {
    this.journal = new FactJournal(directory);
    this.#current = this.#snapshot(config.model, 0);
  }
  #snapshot(model: string, revision: number): ModelSnapshot {
    let client = this.#clients.get(model);
    if (!client) {
      client = new ChatModelClient({ ...this.config, model });
      this.#clients.set(model, client);
    }
    return { modelName: model, revision, maxOutputTokens: this.config.maxOutputTokens, client };
  }
  async initialize() {
    await this.journal.initialize();
    try {
      if (!this.journal.events.length) {
        await this.journal.append("initial", { model: this.config.model });
      }
      for (const event of this.journal.events) {
        if (event.kind === "initial") {
          const model = (event.data as { model?: unknown }).model;
          if (event.seq !== 1 || !modelId(model)) {
            throw new KernelError("corrupt", "模型初始选择原件无效");
          }
          this.#current = this.#snapshot(model, 0);
          continue;
        }
        if (event.seq === 1) throw new KernelError("corrupt", "模型选择缺少初始原件");
        const d = event.data as unknown as SelectionRequest;
        if (!d || !modelCommandId(d.actionId) || !modelId(d.model)) {
          throw new KernelError("corrupt", "模型选择原件无效");
        }
        if (event.kind === "request") {
          if (this.#requests.has(d.actionId)) throw new KernelError("corrupt", "模型选择动作重复");
          this.#requests.set(d.actionId, { actionId: d.actionId, model: d.model });
        } else if (event.kind === "result") {
          const request = this.#requests.get(d.actionId);
          if (
            !request || request.model !== d.model || request.result || request.error ||
            Boolean(d.result) === Boolean(d.error)
          ) {
            throw new KernelError("corrupt", "模型选择回执缺少原请求");
          }
          if (d.result) {
            const r = d.result, changed = this.#current.modelName !== d.model;
            if (
              r.actionId !== d.actionId || r.model !== d.model || r.changed !== changed ||
              r.revision !== this.#current.revision + (changed ? 1 : 0)
            ) {
              throw new KernelError("corrupt", "模型选择修订链无效");
            }
            this.#current = this.#snapshot(r.model, r.revision);
            request.result = r;
          } else {
            if (
              !d.error || !["rejected", "unavailable", "unknown"].includes(d.error.kind) ||
              typeof d.error.message !== "string"
            ) {
              throw new KernelError("corrupt", "模型选择错误回执无效");
            }
            request.error = d.error;
          }
        } else throw new KernelError("corrupt", "未知模型选择原件");
      }
    } catch (error) {
      await this.close();
      throw error;
    }
  }
  snapshot(): ModelSnapshot {
    if (!this.journal.available || this.#closed.signal.aborted) {
      throw new KernelError("unavailable", "模型选择服务不可用");
    }
    return { ...this.#current };
  }
  current() {
    const s = this.snapshot();
    return { model: s.modelName, revision: s.revision };
  }
  async availableModels(): Promise<string[]> {
    const base = new URL(this.config.baseUrl);
    const path = base.pathname.replace(/\/$/, "");
    base.pathname = path.endsWith("/v1") ? `${path}/models` : `${path}/v1/models`;
    try {
      const response = await fetch(base, {
        redirect: "error",
        headers: { authorization: `Bearer ${this.config.apiKey}` },
        signal: AbortSignal.any([this.#closed.signal, AbortSignal.timeout(15_000)]),
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new Error();
      }
      const reader = response.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          size += value.length;
          if (size > 1_048_576) {
            await reader.cancel();
            throw new Error();
          }
          chunks.push(value);
        }
      } finally {
        reader.releaseLock();
      }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.length;
      }
      const raw = JSON.parse(new TextDecoder().decode(bytes));
      if (
        !raw || !Array.isArray(raw.data) ||
        raw.data.some((item: { id?: unknown }) => !item || !modelId(item.id))
      ) throw new Error();
      return [
        ...new Set<string>(
          raw.data.filter((item: { supported_endpoint_types?: unknown }) =>
            item.supported_endpoint_types === undefined ||
            Array.isArray(item.supported_endpoint_types) &&
              item.supported_endpoint_types.includes("openai")
          ).map((item: { id: string }) => item.id),
        ),
      ].sort();
    } catch {
      throw new KernelError("unavailable", "暂时无法取得当前凭据的可用模型，请稍后重试");
    }
  }
  async status() {
    const availableModels = await this.availableModels();
    return { current: this.current(), availableModels };
  }
  receipt(actionId: string): ModelSelectionResult {
    if (!modelCommandId(actionId)) throw new KernelError("rejected", "模型命令编号无效");
    const request = this.#requests.get(actionId);
    if (!request) throw new KernelError("not_found", "没有该模型命令回执");
    if (request.error) throw new KernelError(request.error.kind, request.error.message);
    if (!request.result) throw new KernelError("unknown", "原模型切换尚未确认，请核对原回执");
    return structuredClone(request.result);
  }
  async select(actionId: string, model: string): Promise<ModelSelectionResult> {
    if (!modelCommandId(actionId) || !modelId(model)) {
      throw new KernelError("rejected", "模型命令参数无效");
    }
    const work = this.#tail.then(async () => {
      this.snapshot();
      const old = this.#requests.get(actionId);
      if (old) {
        if (old.model !== model) throw new KernelError("rejected", "同一模型命令不能更改目标");
        return this.receipt(actionId);
      }
      // Availability failures precede admission, so the original command can safely be tried again.
      if (!(await this.availableModels()).includes(model)) {
        throw new KernelError("rejected", "该模型不在当前凭据可用列表中");
      }
      const request: SelectionRequest = { actionId, model };
      const changed = this.#current.modelName !== model;
      const next = this.#snapshot(model, this.#current.revision + (changed ? 1 : 0));
      await this.journal.append("request", request as unknown as Json);
      this.#requests.set(actionId, request);
      const result = { actionId, model, revision: next.revision, changed };
      try {
        await this.journal.append("result", { ...request, result } as unknown as Json);
        request.result = result;
        this.#current = next;
        return structuredClone(result);
      } catch (error) {
        // A failed durable append must retain its uncertain original; never invent a new action.
        throw error instanceof KernelError
          ? error
          : new KernelError("unknown", "模型切换落盘未确认");
      }
    });
    this.#tail = work.catch(() => {});
    return await work;
  }
  async close() {
    this.#closed.abort();
    await this.#tail;
    for (const client of this.#clients.values()) client.close();
    await this.journal.close();
  }
}
