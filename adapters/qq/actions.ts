import { QqClient, type QqSendResult, type QqSendText, validateQqSendText } from "./client.ts";
import { writeAtomic } from "./journal.ts";

export interface SendRecord {
  schema: 1;
  actionId: string;
  fingerprint: string;
  request: QqSendText;
  startedAt: string;
  status: "started" | "finished";
  finishedAt?: string;
  result?: QqSendResult;
  /** 新完成动作校验独立尝试账本；旧动作保持无此字段。 */
  attemptCount?: number;
  /** 仅此策略的新动作可持当前许可重入；旧动作不迁移、不重放。 */
  policy?: { kind: "until_sent_v1"; sequence: number };
}

interface SendAttempt {
  schema: 1;
  actionId: string;
  fingerprint: string;
  attempt: number;
  startedAt: string;
  finishedAt?: string;
  result?: QqSendResult;
  /** 重启观察到原尝试没有结果；不是超时或送达回执。 */
  interruptedAt?: string;
}

export type SendStatus =
  & (
    | { actionId: string; status: "running" }
    | { actionId: string; status: "blocked"; blockedBy: string }
    | { actionId: string; status: "unknown"; reason: "interrupted" }
    | { actionId: string; status: "finished"; result: QqSendResult }
  )
  & { attemptCount?: number };

export class QqActionError extends Error {
  constructor(readonly category: "request" | "conflict" | "storage" | "corrupt" | "unavailable") {
    super(`QQ 动作未完成：${category}，已有回执保留`);
    this.name = "QqActionError";
  }
}
function canonical(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${
    Object.keys(record).filter((k) => record[k] !== undefined).sort()
      .map((k) => `${JSON.stringify(k)}:${canonical(record[k])}`).join(",")
  }}`;
}
async function fingerprint(request: QqSendText): Promise<string> {
  const hash = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(request)));
  return Array.from(new Uint8Array(hash), (byte) => byte.toString(16).padStart(2, "0")).join("");
}
function validId(id: unknown): id is string {
  return typeof id === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(id);
}
function validateId(id: string): void {
  if (!validId(id)) throw new QqActionError("request");
}
function validResult(value: unknown): value is QqSendResult {
  if (value === null || typeof value !== "object") return false;
  const r = value as QqSendResult;
  if (r.status === "sent") return typeof r.messageId === "string" && r.messageId.length > 0;
  if (
    !((r.status === "failed" || r.status === "unknown") && typeof r.attempted === "boolean" &&
      (r.status !== "unknown" || r.attempted) && typeof r.error === "object" && r.error !== null)
  ) return false;
  if (
    ![
      "scope",
      "request",
      "authentication",
      "rate_limit",
      "platform",
      "transport",
      "invalid_response",
      "pending",
      "duplicate",
    ].includes(r.error.category)
  ) return false;
  for (const field of [r.error.httpStatus, r.error.code]) {
    if (field !== undefined && (!Number.isSafeInteger(field) || field < 0)) return false;
  }
  if (
    r.error.transportReason !== undefined &&
    !["timeout", "aborted", "network"].includes(r.error.transportReason)
  ) return false;
  if (r.error.phase !== undefined && !["headers", "body"].includes(r.error.phase)) return false;
  if (
    r.error.elapsedMs !== undefined &&
    (!Number.isSafeInteger(r.error.elapsedMs) || r.error.elapsedMs < 0)
  ) return false;
  return r.error.traceId === undefined ||
    (typeof r.error.traceId === "string" && /^[a-zA-Z0-9_.:-]{1,256}$/.test(r.error.traceId));
}
function protectedText(request: QqSendText): boolean {
  return !request.attachment && request.reply !== undefined && "messageId" in request.reply;
}
function retryable(request: QqSendText, result: QqSendResult): boolean {
  return result.status !== "sent" && result.error.category === "transport" &&
    result.error.transportReason === "timeout" && (!result.attempted || protectedText(request));
}
function aborted(result: QqSendResult): boolean {
  return result.status !== "sent" && result.error.category === "transport" &&
    result.error.transportReason === "aborted";
}
function overallResult(attempts: SendAttempt[]): QqSendResult | undefined {
  let overall: QqSendResult | undefined;
  for (const attempt of attempts) {
    const result = attempt.result;
    if (result && (result.status !== "failed" || overall?.status !== "unknown")) overall = result;
  }
  return overall;
}
function settledResult(attempts: SendAttempt[]): QqSendResult | undefined {
  const result = overallResult(attempts);
  return result?.status === "failed" && attempts.some((a) => a.interruptedAt) ? undefined : result;
}
function conversation(request: QqSendText): string {
  return JSON.stringify([request.destination.kind, request.destination.openId]);
}

/** 一个会话只允许队首先成功；许可暂停、未知和永久失败都保留屏障。 */
export class QqSendActions {
  #records = new Map<string, SendRecord>();
  #inflight = new Map<string, Promise<SendStatus>>();
  #conversations = new Map<string, Promise<void>>();
  #queues = new Map<string, Map<string, number>>();
  #attempts = new Map<string, SendAttempt[]>();
  #nextSequence = 1;
  #retryDelays: readonly number[];
  #stop = new AbortController();
  #lock?: Deno.FsFile;
  #failed = false;
  #closing = false;

  constructor(
    readonly client: QqClient,
    readonly directory: string,
    readonly sendScope: "all" | "private" = "private",
    options: { retryDelaysMs?: readonly number[] } = {},
  ) {
    if (!["all", "private"].includes(sendScope)) throw new QqActionError("request");
    const delays = options.retryDelaysMs ?? [250, 1000, 4000, 10_000, 30_000];
    if (
      !Array.isArray(delays) || !delays.length ||
      delays.some((n) => !Number.isSafeInteger(n) || n < 0 || n > 60_000)
    ) throw new QqActionError("request");
    this.#retryDelays = [...delays];
  }
  get available(): boolean {
    return this.#lock !== undefined && !this.#failed && !this.#closing;
  }
  #available(): void {
    if (!this.available) throw new QqActionError("unavailable");
  }
  #enqueue(record: Pick<SendRecord, "actionId" | "request" | "policy">): void {
    if (!record.policy) return;
    const key = conversation(record.request);
    const queue = this.#queues.get(key) ?? new Map<string, number>();
    queue.set(record.actionId, record.policy.sequence);
    this.#queues.set(key, queue);
  }
  #head(request: QqSendText): string | undefined {
    const queue = this.#queues.get(conversation(request));
    let head: string | undefined, sequence = Infinity;
    for (const [id, seq] of queue ?? []) {
      if (seq < sequence) {
        head = id;
        sequence = seq;
      }
    }
    return head;
  }
  #summary(status: SendStatus): SendStatus {
    const count = this.#attempts.get(status.actionId)?.length ?? 0;
    return count > 1 ? { ...status, attemptCount: count } : status;
  }
  #savedStatus(record: SendRecord): SendStatus {
    if (record.status === "finished") {
      return this.#summary({
        actionId: record.actionId,
        status: "finished",
        result: structuredClone(record.result!),
      });
    }
    const head = record.policy && this.#head(record.request);
    if (head && head !== record.actionId) {
      return this.#summary({ actionId: record.actionId, status: "blocked", blockedBy: head });
    }
    return this.#summary(
      this.#inflight.has(record.actionId)
        ? { actionId: record.actionId, status: "running" }
        : { actionId: record.actionId, status: "unknown", reason: "interrupted" },
    );
  }
  status(actionId: string): SendStatus | undefined {
    this.#available();
    validateId(actionId);
    const record = this.#records.get(actionId);
    return record
      ? this.#savedStatus(record)
      : this.#inflight.has(actionId)
      ? { actionId, status: "running" }
      : undefined;
  }
  records(): SendRecord[] {
    this.#available();
    return Array.from(this.#records.values(), (record) => structuredClone(record));
  }
  async initialize(): Promise<void> {
    if (this.#lock || this.#failed || this.#closing) throw new QqActionError("unavailable");
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(`${this.directory}/records`, { recursive: true, mode: 0o700 });
      lock = await Deno.open(`${this.directory}/writer.lock`, {
        read: true,
        write: true,
        create: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) throw new QqActionError("unavailable");
      const sequences = new Set<number>();
      for await (const entry of Deno.readDir(`${this.directory}/records`)) {
        if (!entry.isFile || !/^[a-zA-Z0-9_-]{1,128}\.json$/.test(entry.name)) {
          throw new QqActionError("corrupt");
        }
        const record: SendRecord = await this.#read(`${this.directory}/records/${entry.name}`);
        validateId(record.actionId);
        validateQqSendText(record.request);
        if (
          record.schema !== 1 || entry.name !== `${record.actionId}.json` ||
          record.fingerprint !== await fingerprint(record.request) ||
          this.#records.has(record.actionId) || !Number.isFinite(Date.parse(record.startedAt)) ||
          !["started", "finished"].includes(record.status) ||
          (record.status === "finished" &&
            (!Number.isFinite(Date.parse(record.finishedAt ?? "")) ||
              !validResult(record.result))) ||
          (record.attemptCount !== undefined &&
            (record.status !== "finished" || !Number.isSafeInteger(record.attemptCount) ||
              record.attemptCount < 0)) ||
          (record.status === "started" &&
            (record.result !== undefined || record.finishedAt !== undefined))
        ) throw new QqActionError("corrupt");
        if (record.policy !== undefined) {
          if (
            !record.policy || record.policy.kind !== "until_sent_v1" ||
            !Number.isSafeInteger(record.policy.sequence) || record.policy.sequence < 1 ||
            record.policy.sequence >= Number.MAX_SAFE_INTEGER ||
            sequences.has(record.policy.sequence)
          ) throw new QqActionError("corrupt");
          sequences.add(record.policy.sequence);
          this.#nextSequence = Math.max(this.#nextSequence, record.policy.sequence + 1);
          if (record.status !== "finished" || record.result?.status !== "sent") {
            this.#enqueue(record);
          }
        }
        this.#records.set(record.actionId, record);
      }
      await this.#restoreAttempts();
      for (const record of this.#records.values()) {
        if (
          record.attemptCount !== undefined &&
          record.attemptCount !== (this.#attempts.get(record.actionId)?.length ?? 0)
        ) throw new QqActionError("corrupt");
      }
      this.#lock = lock;
    } catch (error) {
      lock?.close();
      this.#failed = true;
      if (error instanceof QqActionError) throw error;
      throw new QqActionError("corrupt");
    }
  }
  async #read(path: string) {
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink || stat.size > 2_097_152) throw new QqActionError("corrupt");
    return JSON.parse(await Deno.readTextFile(path));
  }
  async #restoreAttempts(): Promise<void> {
    await Deno.mkdir(`${this.directory}/attempts`, { recursive: true, mode: 0o700 });
    for await (const entry of Deno.readDir(`${this.directory}/attempts`)) {
      const record = this.#records.get(entry.name);
      if (!validId(entry.name) || !entry.isDirectory || !record) throw new QqActionError("corrupt");
      const starts = new Map<number, SendAttempt>();
      const results = new Map<number, { finishedAt: string; result: QqSendResult }>();
      const interrupted = new Map<number, string>();
      for await (const file of Deno.readDir(`${this.directory}/attempts/${entry.name}`)) {
        const match = /^([1-9]\d*)\.(started|result|interrupted)\.json$/.exec(file.name);
        if (!file.isFile || !match || !Number.isSafeInteger(Number(match[1]))) {
          throw new QqActionError("corrupt");
        }
        const value = await this.#read(`${this.directory}/attempts/${entry.name}/${file.name}`);
        const n = Number(match[1]);
        if (
          value?.schema !== 1 || value.actionId !== record.actionId ||
          value.fingerprint !== record.fingerprint || value.attempt !== n
        ) throw new QqActionError("corrupt");
        if (match[2] === "started") {
          if (
            !Number.isFinite(Date.parse(value.startedAt)) || value.result !== undefined ||
            value.finishedAt !== undefined || value.interruptedAt !== undefined
          ) throw new QqActionError("corrupt");
          starts.set(n, value);
        } else if (match[2] === "result") {
          if (!Number.isFinite(Date.parse(value.finishedAt)) || !validResult(value.result)) {
            throw new QqActionError("corrupt");
          }
          results.set(n, value);
        } else {
          if (!record.policy || !Number.isFinite(Date.parse(value.interruptedAt))) {
            throw new QqActionError("corrupt");
          }
          interrupted.set(n, value.interruptedAt);
        }
      }
      const attempts: SendAttempt[] = [];
      for (let n = 1; n <= starts.size; n++) {
        const start = starts.get(n), previous = attempts[n - 2];
        if (
          !start || (n > 1 && !(previous.result && retryable(record.request, previous.result)) &&
            !(record.policy &&
              ((protectedText(record.request) && previous.interruptedAt) ||
                (previous.result && aborted(previous.result) && previous.result.status !== "sent" &&
                  (!previous.result.attempted || protectedText(record.request))))))
        ) throw new QqActionError("corrupt");
        const result = results.get(n), interruptedAt = interrupted.get(n);
        if (result && interruptedAt) throw new QqActionError("corrupt");
        attempts.push({
          ...start,
          ...(result ? { result: result.result, finishedAt: result.finishedAt } : {}),
          ...(interruptedAt ? { interruptedAt } : {}),
        });
      }
      if ([...results.keys(), ...interrupted.keys()].some((n) => !starts.has(n))) {
        throw new QqActionError("corrupt");
      }
      if (attempts.length) {
        if (
          record.status === "finished" &&
          (!attempts.at(-1)!.result ||
            canonical(record.result) !== canonical(overallResult(attempts)))
        ) throw new QqActionError("corrupt");
        this.#attempts.set(record.actionId, attempts);
      }
    }
  }
  async #save(record: SendRecord): Promise<void> {
    try {
      await writeAtomic(`${this.directory}/records/${record.actionId}.json`, record);
    } catch {
      this.#failed = true;
      throw new QqActionError("storage");
    }
    this.#records.set(record.actionId, record);
  }
  /** 每次尝试的开始、真实结果、中断观察各只创建一次，绝不改写已存在事实。 */
  async #attemptFact(
    attempt: SendAttempt,
    kind: "started" | "result" | "interrupted",
  ): Promise<void> {
    const directory = `${this.directory}/attempts/${attempt.actionId}`;
    try {
      await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
      const common = {
        schema: 1,
        actionId: attempt.actionId,
        fingerprint: attempt.fingerprint,
        attempt: attempt.attempt,
      };
      const value = kind === "started"
        ? { ...common, startedAt: attempt.startedAt }
        : kind === "result"
        ? { ...common, finishedAt: attempt.finishedAt, result: attempt.result }
        : { ...common, interruptedAt: attempt.interruptedAt };
      const file = await Deno.open(`${directory}/${attempt.attempt}.${kind}.json`, {
        createNew: true,
        write: true,
        mode: 0o600,
      });
      try {
        const bytes = new TextEncoder().encode(JSON.stringify(value) + "\n");
        let offset = 0;
        while (offset < bytes.length) {
          const n = await file.write(bytes.subarray(offset));
          if (!n) throw new QqActionError("storage");
          offset += n;
        }
        await file.sync();
      } finally {
        file.close();
      }
      for (const path of [directory, `${this.directory}/attempts`]) {
        const handle = await Deno.open(path, { read: true });
        try {
          await handle.sync();
        } finally {
          handle.close();
        }
      }
    } catch {
      this.#failed = true;
      throw new QqActionError("storage");
    }
  }
  async #wait(delay: number): Promise<boolean> {
    if (this.#stop.signal.aborted) return false;
    return await new Promise((resolve) => {
      const finish = (value: boolean) => {
        clearTimeout(timer);
        this.#stop.signal.removeEventListener("abort", stop);
        resolve(value);
      };
      const stop = () => finish(false);
      const timer = setTimeout(() => finish(true), delay);
      this.#stop.signal.addEventListener("abort", stop, { once: true });
    });
  }
  async #transmit(
    started: SendRecord,
    beforeAttempt?: () => Promise<void>,
  ): Promise<QqSendResult | undefined> {
    const attempts = this.#attempts.get(started.actionId) ?? [];
    this.#attempts.set(started.actionId, attempts);
    const last = attempts.at(-1);
    if (last) {
      if (last.result && !retryable(started.request, last.result) && !aborted(last.result)) {
        return settledResult(attempts);
      }
      if (!last.result) {
        if (!last.interruptedAt) {
          const interrupted = { ...last, interruptedAt: new Date().toISOString() };
          await this.#attemptFact(interrupted, "interrupted");
          attempts[attempts.length - 1] = interrupted;
        }
        if (!protectedText(started.request)) return undefined;
      } else if (
        aborted(last.result) && !protectedText(started.request) && last.result.status !== "sent" &&
        last.result.attempted
      ) return undefined;
    }
    if (this.sendScope !== "all" && started.request.destination.kind !== "user") {
      return attempts.length
        ? undefined
        : { status: "failed", attempted: false, error: { category: "scope" } };
    }
    while (!this.#closing) {
      if (this.#failed) throw new QqActionError("storage");
      try {
        await beforeAttempt?.();
      } catch {
        return undefined;
      }
      if (this.#closing) return undefined;
      const n = attempts.length + 1;
      if (!Number.isSafeInteger(n)) {
        this.#failed = true;
        throw new QqActionError("storage");
      }
      const attempt: SendAttempt = {
        schema: 1,
        actionId: started.actionId,
        fingerprint: started.fingerprint,
        attempt: n,
        startedAt: new Date().toISOString(),
      };
      await this.#attemptFact(attempt, "started");
      attempts.push(attempt);
      const result: QqSendResult = JSON.parse(
        JSON.stringify(await this.client.sendText(started.request)),
      );
      const finished = { ...attempt, finishedAt: new Date().toISOString(), result };
      await this.#attemptFact(finished, "result");
      attempts[n - 1] = finished;
      if (aborted(result)) return undefined;
      if (!retryable(started.request, result)) return settledResult(attempts);
      if (!await this.#wait(this.#retryDelays[Math.min(n - 1, this.#retryDelays.length - 1)])) {
        return undefined;
      }
    }
    return undefined;
  }
  async send(
    actionId: string,
    request: QqSendText,
    options: { beforeAttempt?: () => Promise<void> } = {},
  ): Promise<SendStatus> {
    this.#available();
    validateId(actionId);
    const snapshot = structuredClone(request);
    try {
      validateQqSendText(snapshot);
    } catch {
      throw new QqActionError("request");
    }
    if (
      new TextEncoder().encode(JSON.stringify(snapshot)).length > 1_048_576 ||
      (options.beforeAttempt !== undefined && typeof options.beforeAttempt !== "function")
    ) throw new QqActionError("request");
    const existing = this.#inflight.get(actionId);
    if (existing) {
      await existing;
      return await this.send(actionId, snapshot, options);
    }
    const key = conversation(snapshot), previous = this.#records.get(actionId);
    let policy = previous?.policy;
    if (!previous) {
      if (
        !Number.isSafeInteger(this.#nextSequence) || this.#nextSequence >= Number.MAX_SAFE_INTEGER
      ) throw new QqActionError("storage");
      policy = { kind: "until_sent_v1", sequence: this.#nextSequence++ };
      this.#enqueue({ actionId, request: snapshot, policy });
    }
    const preceding = this.#conversations.get(key);
    const operation = (async (): Promise<SendStatus> => {
      const hash = await fingerprint(snapshot);
      if (previous && previous.fingerprint !== hash) throw new QqActionError("conflict");
      if (previous && (previous.status === "finished" || !previous.policy)) {
        return this.#summary(
          previous.status === "finished"
            ? { actionId, status: "finished", result: structuredClone(previous.result!) }
            : { actionId, status: "unknown", reason: "interrupted" },
        );
      }
      const started: SendRecord = previous ??
        {
          schema: 1,
          actionId,
          fingerprint: hash,
          request: snapshot,
          startedAt: new Date().toISOString(),
          status: "started",
          policy,
        };
      // 排队动作在等待前序之前落盘，重启仍可按同步预留序号恢复屏障。
      if (!previous) await this.#save(started);
      await preceding;
      if (this.#failed) throw new QqActionError("storage");
      const head = this.#head(snapshot);
      if (head && head !== actionId) {
        return this.#summary({ actionId, status: "blocked", blockedBy: head });
      }
      if (this.#closing) {
        return this.#summary({ actionId, status: "unknown", reason: "interrupted" });
      }
      const result = await this.#transmit(started, options.beforeAttempt);
      if (!result) return this.#summary({ actionId, status: "unknown", reason: "interrupted" });
      await this.#save({
        ...started,
        status: "finished",
        finishedAt: new Date().toISOString(),
        result,
        attemptCount: this.#attempts.get(actionId)?.length ?? 0,
      });
      if (result.status === "sent") this.#queues.get(key)?.delete(actionId);
      return this.#summary({ actionId, status: "finished", result: structuredClone(result) });
    })();
    const tail = operation.then(() => {}, () => {});
    this.#conversations.set(key, tail);
    this.#inflight.set(actionId, operation);
    try {
      return await operation;
    } finally {
      this.#inflight.delete(actionId);
      if (this.#conversations.get(key) === tail) this.#conversations.delete(key);
    }
  }
  /** 维护先停重试再停RPC；原动作与会话屏障保留供新许可恢复。 */
  requestStop(): void {
    this.#closing = true;
    this.#stop.abort();
  }
  async close(): Promise<void> {
    this.requestStop();
    this.client.close();
    const results = await Promise.allSettled([...this.#inflight.values()]);
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed || results.some((r) => r.status === "rejected")) {
      throw new QqActionError("storage");
    }
  }
}
