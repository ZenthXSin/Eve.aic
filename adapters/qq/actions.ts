import { QqClient, type QqSendResult, type QqSendText, validateQqSendText } from "./client.ts";
import { writeAtomic } from "./journal.ts";

/** 兼容旧调用方的回复参数；新动作统一主动发送，不再分配被动回复序号。 */
export type QqActionSendText = Omit<QqSendText, "reply"> & {
  reply?: { messageId: string; sequence?: number } | { eventId: string };
};

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

export interface SendResolution {
  schema: 1;
  kind: "failed_retained" | "unknown_retained" | "queued_cancelled";
  actionId: string;
  fingerprint: string;
  receiptDigest: string;
  actor: "admin" | "eve";
  resolvedAt: string;
  blockedBy?: string;
}
export interface SendQueueItem {
  actionId: string;
  fingerprint: string;
  receiptDigest?: string;
  sequence: number;
  conversationId: string;
  status: SendStatus;
  canCancel: boolean;
  cancelBlockedReason?: "running" | "unknown" | "not_waiting";
}
export interface SendCancellation {
  actionId: string;
  expectedFingerprint: string;
  expectedReceiptDigest: string;
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
    | {
      actionId: string;
      status: "cancelled";
      resolution: SendResolution;
      originalResult?: QqSendResult;
    }
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
  if (
    r.error.retryAfterMs !== undefined &&
    (!Number.isSafeInteger(r.error.retryAfterMs) || r.error.retryAfterMs < 0 ||
      r.error.retryAfterMs > 3_600_000)
  ) return false;
  if (
    r.error.failureStage !== undefined &&
    !["auth", "upload", "message"].includes(r.error.failureStage)
  ) {
    return false;
  }
  return r.error.traceId === undefined ||
    (typeof r.error.traceId === "string" && /^[a-zA-Z0-9_.:-]{1,256}$/.test(r.error.traceId));
}
function legacyProtectedText(request: QqSendText): boolean {
  return !request.attachment && request.reply !== undefined && "messageId" in request.reply;
}
function retryable(result: QqSendResult): boolean {
  if (
    result.status === "failed" && ["authentication", "rate_limit"].includes(result.error.category)
  ) {
    return true;
  }
  return result.status !== "sent" && result.error.category === "transport" &&
    result.error.transportReason === "timeout" && !result.attempted;
}
/** Only validates immutable historical attempt chains; never authorizes a new send. */
function legacyRetryable(request: QqSendText, result: QqSendResult): boolean {
  return retryable(result) ||
    result.status !== "sent" && result.error.category === "transport" &&
      result.error.transportReason === "timeout" && legacyProtectedText(request);
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
  // 首个异步哈希/落盘前同步预留，保证并发动作准入及会话顺序稳定。
  #admissions = new Map<string, QqSendText>();
  #inflight = new Map<string, Promise<SendStatus>>();
  #conversations = new Map<string, Promise<void>>();
  #queues = new Map<string, Map<string, number>>();
  #attempts = new Map<string, SendAttempt[]>();
  #resolutions = new Map<string, SendResolution>();
  #resolutionTail: Promise<unknown> = Promise.resolve();
  #resolving = new Set<string>();
  #queueWake = new Map<string, () => void>();
  #queueListeners = new Set<() => void>();

  /** 已落盘的取消或真实送达释放队列后，通知调用方用原动作编号重新核对等待项。 */
  onQueueChange(listener: () => void): () => void {
    this.#queueListeners.add(listener);
    return () => {
      this.#queueListeners.delete(listener);
    };
  }
  #queueChanged() {
    queueMicrotask(() => {
      for (const listener of this.#queueListeners) {
        try {
          listener();
        } catch { /* 观察者不能改变发送事实或取消回执。 */ }
      }
    });
  }
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
    const resolution = this.#resolutions.get(record.actionId);
    if (resolution) {
      return this.#summary({
        actionId: record.actionId,
        status: "cancelled",
        resolution: structuredClone(resolution),
        ...(record.result ? { originalResult: structuredClone(record.result) } : {}),
      });
    }
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
  #isQueued(record: SendRecord): boolean {
    return record.status === "started" && !!record.policy &&
      !(this.#attempts.get(record.actionId)?.length) &&
      this.#savedStatus(record).status === "blocked";
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
  async #receipt(record: SendRecord): Promise<string> {
    try {
      const path = `${this.directory}/records/${record.actionId}.json`;
      const stat = await Deno.lstat(path);
      if (!stat.isFile || stat.isSymlink || stat.size > 2_097_152) throw new Error();
      const bytes = await Deno.readFile(path);
      if (canonical(JSON.parse(new TextDecoder().decode(bytes))) !== canonical(record)) {
        throw new Error();
      }
      const hash = await crypto.subtle.digest("SHA-256", bytes);
      return Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, "0")).join("");
    } catch {
      if (
        (this.#inflight.has(record.actionId) && !this.#isQueued(record)) ||
        this.#records.get(record.actionId) !== record
      ) {
        throw new QqActionError("conflict");
      }
      this.#failed = true;
      throw new QqActionError("corrupt");
    }
  }
  async #verifyAttempts(record: SendRecord) {
    const attempts = this.#attempts.get(record.actionId) ?? [];
    const expected = new Map<string, unknown>();
    for (const attempt of attempts) {
      const common = {
        schema: 1,
        actionId: record.actionId,
        fingerprint: record.fingerprint,
        attempt: attempt.attempt,
      };
      expected.set(`${attempt.attempt}.started.json`, { ...common, startedAt: attempt.startedAt });
      if (attempt.result) {
        expected.set(`${attempt.attempt}.result.json`, {
          ...common,
          finishedAt: attempt.finishedAt,
          result: attempt.result,
        });
      }
      if (attempt.interruptedAt) {
        expected.set(`${attempt.attempt}.interrupted.json`, {
          ...common,
          interruptedAt: attempt.interruptedAt,
        });
      }
    }
    try {
      const path = `${this.directory}/attempts/${record.actionId}`;
      const stat = await Deno.lstat(path);
      if (!stat.isDirectory || stat.isSymlink) throw new Error();
      const actual = new Set<string>();
      for await (const entry of Deno.readDir(path)) {
        if (
          !entry.isFile || !expected.has(entry.name) ||
          canonical(await this.#read(`${path}/${entry.name}`)) !==
            canonical(expected.get(entry.name))
        ) throw new Error();
        actual.add(entry.name);
      }
      if (actual.size !== expected.size) throw new Error();
    } catch (error) {
      if (error instanceof Deno.errors.NotFound && !expected.size) return;
      this.#failed = true;
      throw new QqActionError("corrupt");
    }
  }
  #eligibility(
    record: SendRecord,
    reserved = false,
  ): {
    kind?: SendResolution["kind"];
    reason?: SendQueueItem["cancelBlockedReason"];
    blockedBy?: string;
  } {
    if (
      (this.#inflight.has(record.actionId) && !this.#isQueued(record)) ||
      (!reserved && this.#resolving.has(record.actionId))
    ) return { reason: "running" };
    if (
      record.status === "finished" && record.result?.status === "failed" && record.policy &&
      this.#head(record.request) === record.actionId
    ) return { kind: "failed_retained" };
    const status = this.#savedStatus(record);
    if (
      this.#head(record.request) === record.actionId && record.policy &&
      (status.status === "unknown" ||
        (status.status === "finished" && status.result.status === "unknown"))
    ) return { kind: "unknown_retained" };
    if (
      record.status === "started" && record.policy && status.status === "blocked" &&
      !(this.#attempts.get(record.actionId)?.length)
    ) return { kind: "queued_cancelled", blockedBy: status.blockedBy };
    return {
      reason: status.status === "unknown" || record.result?.status === "unknown"
        ? "unknown"
        : "not_waiting",
    };
  }
  async listSendQueue(
    args: {
      afterSequence?: number;
      limit?: number;
      conversationId?: string;
      failedHeadsOnly?: boolean;
    } = {},
  ) {
    this.#available();
    const after = args.afterSequence ?? 0, limit = args.limit ?? 8;
    if (
      !Number.isSafeInteger(after) || after < 0 || !Number.isSafeInteger(limit) || limit < 1 ||
      limit > 32 ||
      (args.conversationId !== undefined && !/^qq:(user|group):.{1,512}$/.test(args.conversationId))
    ) throw new QqActionError("request");
    const eligible = [...this.#records.values()].filter((record) => {
      if (
        !record.policy || record.policy.sequence <= after ||
        this.#resolutions.has(record.actionId) || record.result?.status === "sent"
      ) return false;
      if (
        args.conversationId !== undefined &&
        args.conversationId !==
          `qq:${record.request.destination.kind}:${record.request.destination.openId}`
      ) return false;
      return !args.failedHeadsOnly ||
        (this.#head(record.request) === record.actionId &&
          ["failed_retained", "unknown_retained"].includes(this.#eligibility(record).kind ?? ""));
    }).sort((a, b) => a.policy!.sequence - b.policy!.sequence);
    const items: SendQueueItem[] = [];
    for (let record of eligible.slice(0, limit)) {
      let receiptDigest: string | undefined;
      for (;;) {
        if (this.#inflight.has(record.actionId) && !this.#isQueued(record)) break;
        try {
          receiptDigest = await this.#receipt(record);
          break;
        } catch (error) {
          if (!(error instanceof QqActionError) || error.category !== "conflict") throw error;
          record = this.#records.get(record.actionId)!;
        }
      }
      const eligibility = this.#eligibility(record);
      items.push({
        actionId: record.actionId,
        fingerprint: record.fingerprint,
        ...(receiptDigest ? { receiptDigest } : {}),
        sequence: record.policy!.sequence,
        conversationId:
          `qq:${record.request.destination.kind}:${record.request.destination.openId}`,
        status: this.#savedStatus(record),
        canCancel: !!eligibility.kind && !!receiptDigest,
        ...(eligibility.reason ? { cancelBlockedReason: eligibility.reason } : {}),
      });
    }
    return {
      items,
      ...(eligible.length > limit
        ? { next: { ...args, afterSequence: items.at(-1)!.sequence } }
        : {}),
    };
  }
  cancelSend(
    args: SendCancellation,
    options: {
      actor?: "admin" | "eve";
      failedHeadOnly?: boolean;
      checkpoint?: () => Promise<void>;
    } = {},
  ): Promise<SendStatus> {
    const operation = this.#resolutionTail.then(async () => {
      this.#available();
      if (
        !args || !validId(args.actionId) || typeof args.expectedFingerprint !== "string" ||
        typeof args.expectedReceiptDigest !== "string" ||
        !/^[a-f0-9]{64}$/.test(args.expectedFingerprint) ||
        !/^[a-f0-9]{64}$/.test(args.expectedReceiptDigest) || Object.keys(args).some((key) =>
          !["actionId", "expectedFingerprint", "expectedReceiptDigest"].includes(key)
        )
      ) {
        throw new QqActionError("request");
      }
      const record = this.#records.get(args.actionId);
      if (!record || record.fingerprint !== args.expectedFingerprint) {
        throw new QqActionError("conflict");
      }
      if (this.#inflight.has(record.actionId) && !this.#isQueued(record)) {
        throw new QqActionError("conflict");
      }
      this.#resolving.add(args.actionId);
      try {
        const receiptDigest = await this.#receipt(record);
        if (receiptDigest !== args.expectedReceiptDigest) {
          throw new QqActionError("conflict");
        }
        await this.#verifyAttempts(record);
        const previous = this.#resolutions.get(args.actionId);
        if (previous) {
          return this.#savedStatus(record);
        }
        const eligibility = this.#eligibility(record, true);
        if (
          !eligibility.kind || (options.failedHeadOnly &&
            !["failed_retained", "unknown_retained"].includes(eligibility.kind))
        ) {
          throw new QqActionError("conflict");
        }
        await options.checkpoint?.();
        const resolution: SendResolution = {
          schema: 1,
          kind: eligibility.kind,
          actionId: record.actionId,
          fingerprint: record.fingerprint,
          receiptDigest,
          actor: options.actor ?? "eve",
          resolvedAt: new Date().toISOString(),
          ...(eligibility.blockedBy ? { blockedBy: eligibility.blockedBy } : {}),
        };
        const path = `${this.directory}/resolutions/${record.actionId}.json`;
        try {
          const file = await Deno.open(path, { createNew: true, write: true, mode: 0o600 });
          try {
            const bytes = new TextEncoder().encode(JSON.stringify(resolution) + "\n");
            let offset = 0;
            while (offset < bytes.length) {
              const count = await file.write(bytes.subarray(offset));
              if (!count) {
                throw new Error();
              }
              offset += count;
            }
            await file.sync();
          } finally {
            file.close();
          }
          const directory = await Deno.open(`${this.directory}/resolutions`, { read: true });
          try {
            await directory.sync();
          } finally {
            directory.close();
          }
        } catch {
          this.#failed = true;
          throw new QqActionError("storage");
        }
        this.#resolutions.set(record.actionId, resolution);
        this.#queues.get(conversation(record.request))?.delete(record.actionId);
        this.#queueWake.get(record.actionId)?.();
        this.#queueChanged();
        return this.#savedStatus(record);
      } finally {
        this.#resolving.delete(args.actionId);
      }
    });
    this.#resolutionTail = operation.catch(() => {});
    return operation;
  }
  async #restoreResolutions() {
    const directory = `${this.directory}/resolutions`;
    await Deno.mkdir(directory, { recursive: true, mode: 0o700 });
    const resolutions: SendResolution[] = [];
    for await (const entry of Deno.readDir(directory)) {
      if (!entry.isFile || !/^[a-zA-Z0-9_-]{1,128}\.json$/.test(entry.name)) {
        throw new QqActionError("corrupt");
      }
      const resolution: SendResolution = await this.#read(`${directory}/${entry.name}`);
      const record = this.#records.get(resolution.actionId);
      if (
        !record?.policy || entry.name !== `${record.actionId}.json` || resolution.schema !== 1 ||
        !["failed_retained", "unknown_retained", "queued_cancelled"].includes(resolution.kind) ||
        !["admin", "eve"].includes(resolution.actor) ||
        !Number.isFinite(Date.parse(resolution.resolvedAt)) ||
        resolution.fingerprint !== record.fingerprint ||
        resolution.receiptDigest !== await this.#receipt(record) ||
        Object.keys(resolution).some((key) =>
          ![
            "schema",
            "kind",
            "actionId",
            "fingerprint",
            "receiptDigest",
            "actor",
            "resolvedAt",
            "blockedBy",
          ].includes(key)
        )
      ) throw new QqActionError("corrupt");
      if (resolution.kind === "failed_retained") {
        if (
          record.status !== "finished" || record.result?.status !== "failed" ||
          resolution.blockedBy !== undefined
        ) throw new QqActionError("corrupt");
      } else if (resolution.kind === "unknown_retained") {
        if (
          resolution.blockedBy !== undefined ||
          !(record.status === "started" ||
            (record.status === "finished" && record.result?.status === "unknown"))
        ) throw new QqActionError("corrupt");
      } else {
        const preceding = this.#records.get(resolution.blockedBy ?? "");
        if (
          record.status !== "started" || (this.#attempts.get(record.actionId)?.length ?? 0) !== 0 ||
          !preceding?.policy || preceding.policy.sequence >= record.policy.sequence ||
          conversation(preceding.request) !== conversation(record.request)
        ) throw new QqActionError("corrupt");
      }
      resolutions.push(resolution);
    }
    resolutions.sort((a, b) =>
      this.#records.get(a.actionId)!.policy!.sequence -
      this.#records.get(b.actionId)!.policy!.sequence
    );
    for (const resolution of resolutions) {
      const record = this.#records.get(resolution.actionId)!;
      if (
        resolution.kind !== "queued_cancelled" && this.#head(record.request) !== record.actionId
      ) {
        throw new QqActionError("corrupt");
      }
      this.#resolutions.set(record.actionId, resolution);
      this.#queues.get(conversation(record.request))?.delete(record.actionId);
    }
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
      await this.#restoreResolutions();
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
          !start ||
          (n > 1 && !(previous.result && legacyRetryable(record.request, previous.result)) &&
            !(record.policy &&
              ((legacyProtectedText(record.request) && previous.interruptedAt) ||
                (previous.result && aborted(previous.result) && previous.result.status !== "sent" &&
                  (!previous.result.attempted || legacyProtectedText(record.request))))))
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
      if (!last.result) {
        if (!last.interruptedAt) {
          const interrupted = { ...last, interruptedAt: new Date().toISOString() };
          await this.#attemptFact(interrupted, "interrupted");
          attempts[attempts.length - 1] = interrupted;
        }
        return undefined;
      }
      const outcome = settledResult(attempts);
      // A durable platform receipt is authoritative even if the process died
      // before the action's final summary was committed.
      if (outcome?.status === "sent") return outcome;
      // Legacy passive retries could end in an explicit auth/rate rejection after
      // an earlier unknown send. A later rejection cannot prove that earlier
      // attempt was not delivered, and active sending has no safe replay key.
      if (
        attempts.some((attempt) =>
          attempt.interruptedAt || !attempt.result || attempt.result.status === "unknown" ||
          (attempt.result.status === "failed" && attempt.result.attempted &&
            aborted(attempt.result))
        )
      ) {
        return outcome?.status === "unknown" ? outcome : undefined;
      }
      if (!retryable(last.result) && !aborted(last.result)) return settledResult(attempts);
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
      if (!retryable(result)) return settledResult(attempts);
      const baseDelay = this.#retryDelays[Math.min(n - 1, this.#retryDelays.length - 1)];
      if (
        !await this.#wait(
          baseDelay + (result.status === "failed" ? result.error.retryAfterMs ?? 0 : 0),
        )
      ) {
        return undefined;
      }
    }
    return undefined;
  }
  #admit(actionId: string, request: QqActionSendText): QqSendText {
    const snapshot = structuredClone(request);
    const previous = this.#records.get(actionId)?.request ?? this.#admissions.get(actionId);
    // Pin the caller format hint for replay identity; the transport always renders plain text.
    if (snapshot && typeof snapshot === "object" && snapshot.format === undefined) {
      if (previous?.format !== undefined) snapshot.format = previous.format;
      else if (!previous) snapshot.format = "plain";
    }
    // Validate legacy-shaped caller input before ignoring its passive reply metadata.
    const validation = structuredClone(snapshot);
    if (
      validation?.reply && typeof validation.reply === "object" &&
      "messageId" in validation.reply && validation.reply.sequence === undefined
    ) {
      validation.reply.sequence = 1;
    }
    validateQqSendText(validation as QqSendText);
    delete snapshot.reply;
    if (previous) {
      const activePrevious = structuredClone(previous);
      delete activePrevious.reply;
      if (canonical(activePrevious) !== canonical(snapshot)) throw new QqActionError("conflict");
      // Existing fingerprints are immutable. An upgraded caller omitting reply still
      // addresses the original finished, cancelled or interrupted action.
      return structuredClone(previous);
    }
    return snapshot as QqSendText;
  }
  async send(
    actionId: string,
    request: QqActionSendText,
    options: { beforeAttempt?: () => Promise<void> } = {},
  ): Promise<SendStatus> {
    this.#available();
    validateId(actionId);
    if (this.#resolving.has(actionId)) {
      await this.#resolutionTail;
      return await this.send(actionId, request, options);
    }
    let snapshot: QqSendText;
    try {
      snapshot = this.#admit(actionId, request);
    } catch (error) {
      if (error instanceof QqActionError) throw error;
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
      this.#admissions.set(actionId, snapshot);
      this.#enqueue({ actionId, request: snapshot, policy });
    }
    const preceding = this.#conversations.get(key);
    let wake!: () => void;
    const cancelled = new Promise<void>((resolve) => {
      wake = resolve;
    });
    this.#queueWake.set(actionId, wake);
    const operation = (async (): Promise<SendStatus> => {
      const hash = await fingerprint(snapshot);
      if (previous && previous.fingerprint !== hash) throw new QqActionError("conflict");
      if (previous && this.#resolutions.has(actionId)) return this.#savedStatus(previous);
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
      await Promise.race([preceding, cancelled]);
      if (this.#resolving.has(actionId)) await this.#resolutionTail;
      if (this.#resolutions.has(actionId)) return this.#savedStatus(started);
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
      if (result.status === "sent") {
        this.#queues.get(key)?.delete(actionId);
        this.#queueChanged();
      }
      return this.#summary({ actionId, status: "finished", result: structuredClone(result) });
    })();
    const tail = operation.then(() => {}, () => {});
    this.#conversations.set(key, tail);
    this.#inflight.set(actionId, operation);
    try {
      return await operation;
    } finally {
      this.#admissions.delete(actionId);
      this.#inflight.delete(actionId);
      this.#queueWake.delete(actionId);
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
    await this.#resolutionTail;
    this.client.close();
    const results = await Promise.allSettled([...this.#inflight.values()]);
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed || results.some((r) => r.status === "rejected")) {
      throw new QqActionError("storage");
    }
  }
}
