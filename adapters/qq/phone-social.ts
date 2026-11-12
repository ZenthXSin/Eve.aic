// 显式社交动作的冻结请求与静默窗口；不合成入站，也不在传输层暗中拆消息。
import type { PhoneSendBurst, PhoneWaitForMessages } from "../../contracts/phone.ts";
import { canonical, fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import { QqActionError } from "./actions.ts";
import { writeAtomic } from "./journal.ts";
import { splitForQQ } from "./text-format.ts";

export interface BurstManifest {
  schema: 1;
  fingerprint: string;
  actionId: string;
  conversationId: string;
  messages: string[];
  splitLongText: boolean;
  segments: string[];
  childActionIds: string[];
}

function validateBurst(args: PhoneSendBurst) {
  if (
    !args || typeof args !== "object" ||
    Object.keys(args).some((key) =>
      !["actionId", "conversationId", "messages", "splitLongText", "resumeActionId"].includes(key)
    ) ||
    typeof args.actionId !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(args.actionId) ||
    (args.resumeActionId !== undefined &&
      (typeof args.resumeActionId !== "string" ||
        !/^[a-zA-Z0-9_-]{1,128}$/.test(args.resumeActionId))) ||
    typeof args.conversationId !== "string" ||
    !/^qq:(user|group):.{1,512}$/.test(args.conversationId) ||
    !Array.isArray(args.messages) || args.messages.length < 1 || args.messages.length > 4 ||
    args.messages.some((text) => typeof text !== "string" || !text.trim()) ||
    (args.splitLongText !== undefined && typeof args.splitLongText !== "boolean") ||
    new TextEncoder().encode(JSON.stringify(args.messages)).length > 262_144
  ) throw new QqActionError("request");
}

export function burstSegments(args: PhoneSendBurst): string[] {
  validateBurst(args);
  let segments: string[];
  try {
    segments = args.splitLongText
      ? args.messages.flatMap((text) => splitForQQ(text))
      : [...args.messages];
  } catch {
    throw new KernelError("rejected", "分条文字超过四条；请自行整理，原文未截断也未发送");
  }
  if (segments.length > 4) {
    throw new KernelError("rejected", "分条文字超过四条；请自行整理，原文未截断也未发送");
  }
  return segments;
}

/** 只读原manifest，校验固定子编号、无损正文与完整性，不随新分词规则重算segments。 */
export async function readBurst(
  directory: string,
  actionId: string,
): Promise<BurstManifest | undefined> {
  if (typeof actionId !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(actionId)) {
    throw new QqActionError("request");
  }
  const hash = await fingerprint(actionId),
    folder = `${directory}/bursts`,
    path = `${folder}/${hash}.json`;
  try {
    const parent = await Deno.lstat(folder);
    if (!parent.isDirectory || parent.isSymlink) throw new QqActionError("corrupt");
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink || stat.size > 1_048_576) throw new QqActionError("corrupt");
    const stored = JSON.parse(await Deno.readTextFile(path)) as BurstManifest;
    if (
      !stored || stored.schema !== 1 || stored.actionId !== actionId ||
      !Array.isArray(stored.messages) || !Array.isArray(stored.segments) ||
      stored.segments.length < 1 || stored.segments.length > 4 ||
      stored.segments.some((text) => typeof text !== "string" || !text.trim()) ||
      !Array.isArray(stored.childActionIds) ||
      canonical(stored.childActionIds) !==
        canonical(stored.segments.map((_, i) => `burst_${hash}_${i + 1}`))
    ) throw new QqActionError("corrupt");
    validateBurst({
      actionId: stored.actionId,
      conversationId: stored.conversationId,
      messages: stored.messages,
      splitLongText: stored.splitLongText,
    });
    if (
      stored.segments.join("") !== stored.messages.join("") ||
      (!stored.splitLongText && canonical(stored.messages) !== canonical(stored.segments))
    ) throw new QqActionError("corrupt");
    const { fingerprint: checksum, ...body } = stored;
    if (checksum !== await fingerprint(body as unknown as Json)) throw new QqActionError("corrupt");
    return stored;
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) return undefined;
    if (error instanceof QqActionError && error.category === "corrupt") throw error;
    throw new QqActionError(
      error instanceof SyntaxError || error instanceof QqActionError ? "corrupt" : "storage",
    );
  }
}

/** 父请求先持久化，再允许任一子动作准入；恢复只复用既存manifest。 */
export async function admitBurst(directory: string, args: PhoneSendBurst): Promise<BurstManifest> {
  validateBurst(args);
  const parentId = args.resumeActionId ?? args.actionId;
  const stored = await readBurst(directory, parentId);
  if (stored) {
    if (
      stored.conversationId !== args.conversationId ||
      stored.splitLongText !== (args.splitLongText ?? false) ||
      canonical(stored.messages) !== canonical(args.messages)
    ) throw new QqActionError("conflict");
    return stored;
  }
  if (args.resumeActionId !== undefined) {
    throw new KernelError("not_found", "原分条请求不存在，未创建或发送新动作");
  }
  const segments = burstSegments(args), hash = await fingerprint(parentId);
  const body = {
    schema: 1 as const,
    actionId: parentId,
    conversationId: args.conversationId,
    messages: [...args.messages],
    splitLongText: args.splitLongText ?? false,
    segments,
    childActionIds: segments.map((_, i) => `burst_${hash}_${i + 1}`),
  };
  const manifest = { ...body, fingerprint: await fingerprint(body) };
  const folder = `${directory}/bursts`, path = `${folder}/${hash}.json`;
  try {
    await Deno.mkdir(folder, { recursive: true, mode: 0o700 });
    const parent = await Deno.lstat(folder);
    if (!parent.isDirectory || parent.isSymlink) throw new QqActionError("corrupt");
    await writeAtomic(path, manifest);
    return manifest;
  } catch (error) {
    if (error instanceof QqActionError) throw error;
    throw new QqActionError("storage");
  }
}

export interface IncomingObservation {
  seq: number;
  receivedAt: string;
}
export interface QuietWaitOptions {
  signal?: AbortSignal;
  checkpoint?: () => Promise<void>;
  now?: () => number;
  sleep?: (ms: number, signal?: AbortSignal) => Promise<void>;
}
async function pause(ms: number, signal?: AbortSignal) {
  if (signal?.aborted) return;
  await new Promise<void>((resolve) => {
    const done = () => {
      clearTimeout(timer);
      signal?.removeEventListener("abort", done);
      resolve();
    };
    const timer = setTimeout(done, ms);
    signal?.addEventListener("abort", done, { once: true });
  });
}

/** 单次有界等待，只使用本地真实入站接收时间；每个调用有自己的定时器。 */
export async function waitForQuiet(
  args: PhoneWaitForMessages,
  latest: () => IncomingObservation | undefined,
  options: QuietWaitOptions = {},
) {
  const quietMs = args.quietMs ?? 10_000, maxWaitMs = args.maxWaitMs ?? 60_000;
  if (
    !args || typeof args !== "object" ||
    Object.keys(args).some((key) =>
      !["conversationId", "purpose", "quietMs", "maxWaitMs"].includes(key)
    ) ||
    typeof args.conversationId !== "string" ||
    !/^qq:(user|group):.{1,512}$/.test(args.conversationId) ||
    args.purpose !== "reply" || !Number.isSafeInteger(quietMs) || quietMs < 1 || quietMs > 60_000 ||
    !Number.isSafeInteger(maxWaitMs) || maxWaitMs < 1 || maxWaitMs > 120_000
  ) throw new QqActionError("request");
  const now = options.now ?? Date.now, sleep = options.sleep ?? pause;
  const started = now(), deadline = started + maxWaitMs;
  let observed: IncomingObservation | undefined, quietSince = started;
  while (true) {
    const at = now(), incoming = latest();
    if (incoming && incoming.seq !== observed?.seq) {
      const received = Date.parse(incoming.receivedAt);
      // 无效本地时间不能伪装已静默；新观察到的异常时间从本次观察开始计时。
      quietSince = Number.isFinite(received) ? Math.min(at, received) : at;
      observed = incoming;
    }
    const result = (reason: "quiet" | "max_wait" | "interrupted") => ({
      reason,
      waitedMs: Math.max(0, at - started),
      lastSourceSeq: observed?.seq,
    });
    if (options.signal?.aborted) return result("interrupted");
    await options.checkpoint?.();
    if (at >= deadline) return result("max_wait");
    if (at >= quietSince + quietMs) return result("quiet");
    await sleep(Math.min(250, deadline - at, quietSince + quietMs - at), options.signal);
  }
}
