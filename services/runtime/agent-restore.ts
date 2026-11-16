import type { AgentConfig, AgentJob, AgentMessagePreview } from "../../contracts/agents.ts";
import type { Message, ModelResponse } from "../../contracts/model.ts";
import { KernelError } from "../../kernel/protocol.ts";
import type { FactEvent, FactJournal } from "../../sdk/fact-journal.ts";
import { agentDeliveries } from "./agent-delegation.ts";

export interface BlobRef {
  hash: string;
}
export interface StoredMessage extends AgentMessagePreview {
  body: BlobRef;
}
export interface StoredJob extends AgentJob {
  config: AgentConfig;
  transcript: BlobRef[];
  cancelled: boolean;
}
export interface Session {
  id: string;
  parentAgentId?: string;
  config: AgentConfig;
  closed: boolean;
  readThrough: number;
  messages: number[];
  replies: number[];
  jobs: string[];
  unread: number;
  pending: number;
}
export function createAgentState() {
  return {
    sessions: new Map<string, Session>(),
    jobs: new Map<string, StoredJob>(),
    messages: new Map<number, StoredMessage>(),
    notifications: [] as number[],
  };
}
export type AgentState = ReturnType<typeof createAgentState>;

/** The same metadata projection is used for live commits and ordered journal replay. */
export function applyAgentEvent(state: AgentState, event: FactEvent) {
  const d = event.data as Record<string, unknown>;
  if (event.kind === "created") {
    const config = d.config as AgentConfig, id = String(d.agentId);
    if (state.sessions.has(id)) throw new KernelError("corrupt", "子代理标识重复");
    if (
      d.parentAgentId !== undefined && (
        typeof d.parentAgentId !== "string" ||
        state.sessions.get(d.parentAgentId)?.config.role !== "general" || config.role === "general"
      )
    ) throw new KernelError("corrupt", "子代理会话派工层级无效");
    state.sessions.set(id, {
      id,
      ...(d.parentAgentId === undefined ? {} : { parentAgentId: String(d.parentAgentId) }),
      config,
      closed: false,
      readThrough: 0,
      messages: [],
      replies: [],
      jobs: [],
      unread: 0,
      pending: 0,
    });
    return;
  }
  const session = state.sessions.get(String(d.agentId));
  if (!session) throw new KernelError("corrupt", "子代理事件缺少会话");
  const job = () => {
    const value = state.jobs.get(String(d.jobId));
    if (!value || value.agentId !== session.id) {
      throw new KernelError("corrupt", "子代理事件缺少配对任务");
    }
    return value;
  };
  if (event.kind === "configured") session.config = d.config as AgentConfig;
  else if (event.kind === "read") {
    session.readThrough = Number(d.throughSeq);
    let lo = 0, hi = session.replies.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (session.replies[mid] <= session.readThrough) lo = mid + 1;
      else hi = mid;
    }
    session.unread = session.replies.length - lo;
  } else if (event.kind === "closed") session.closed = true;
  else if (event.kind === "accepted") {
    const accepted = structuredClone(d.job) as StoredJob;
    if (state.jobs.has(accepted.jobId)) throw new KernelError("corrupt", "子代理任务重复");
    if (accepted.agentId !== session.id) throw new KernelError("corrupt", "子代理任务会话不符");
    if (accepted.parentJobId) {
      const parent = state.jobs.get(accepted.parentJobId);
      if (
        !parent || parent.config.role !== "general" || parent.replySeq ||
        session.parentAgentId !== parent.agentId || accepted.config.role === "general" ||
        accepted.rootJobId !== (parent.rootJobId ?? parent.jobId)
      ) throw new KernelError("corrupt", "子代理派工层级或归属无效");
      (parent.childJobIds ??= []).push(accepted.jobId);
    }
    accepted.requestSeq = event.seq;
    state.jobs.set(accepted.jobId, accepted);
    session.jobs.push(accepted.jobId);
    session.pending++;
    const message = { ...(d.message as StoredMessage), seq: event.seq, at: event.at };
    state.messages.set(event.seq, message);
    session.messages.push(event.seq);
  } else if (event.kind === "started") job().status = "running";
  else if (event.kind === "step") job().transcript.push(d.body as BlobRef);
  else if (event.kind === "cancelled") job().cancelled = true;
  else if (event.kind === "reply") {
    const replied = job(),
      message = { ...(d.message as StoredMessage), seq: event.seq, at: event.at };
    if (replied.replySeq) throw new KernelError("corrupt", "子代理回复重复");
    replied.status = message.status;
    session.pending--;
    session.replies.push(event.seq);
    if (event.seq > session.readThrough) session.unread++;
    replied.replySeq = event.seq;
    replied.error = d.error as AgentJob["error"];
    state.messages.set(event.seq, message);
    session.messages.push(event.seq);
    // Specialized deliveries go to their general coordinator, not Eve's inbox.
    if (!replied.parentJobId) state.notifications.push(event.seq);
  } else throw new KernelError("corrupt", "子代理事件类型无效");
}

/** Cursor into a durable transcript. Receipts pair in execution order, not just by count. */
export function transcriptCursor(transcript: Message[]) {
  let step = 0, completed = 0, assistant: Message | undefined;
  for (const message of transcript) {
    if (!message || typeof message.content !== "string") {
      throw new KernelError("corrupt", "子代理步骤原件无效");
    }
    if (message.role === "assistant") {
      if (
        assistant && (!assistant.toolCalls?.length || completed !== assistant.toolCalls.length) ||
        message.toolCalls !== undefined && !Array.isArray(message.toolCalls)
      ) throw new KernelError("corrupt", "子代理工具回执未配对");
      assistant = message;
      completed = 0;
      step++;
    } else if (
      message.role === "user" && agentDeliveries(message) && assistant &&
      (!assistant.toolCalls?.length || completed === assistant.toolCalls.length)
    ) {
      assistant = undefined;
      completed = 0;
    } else if (
      message.role === "tool" && assistant?.toolCalls?.[completed] &&
      message.toolCallId === assistant.toolCalls[completed].id
    ) completed++;
    else throw new KernelError("corrupt", "子代理步骤原件无效或工具回执未配对");
  }
  return { step, completed, assistant };
}

export function assistantMessage(response: ModelResponse): Message {
  return {
    role: "assistant",
    content: response.content,
    ...(response.toolCalls.length ? { toolCalls: response.toolCalls } : {}),
    ...(response.modelOutput ? { modelOutput: response.modelOutput } : {}),
  };
}

interface RestoreHooks {
  validateConfig(config: AgentConfig): void;
  read<T>(body: BlobRef): Promise<T>;
  result?(id: string): ModelResponse | undefined;
  step(job: StoredJob, message: Message): Promise<void>;
  finish(job: StoredJob, status: StoredJob["status"], text: string): Promise<void>;
  canFinish?(job: StoredJob, transcript: Message[]): boolean;
}

/** Validate all originals before changing the journal or admitting resumed work.
 * Final assistant steps (or saved model originals) close interrupted reply commits;
 * all other live jobs remain pending for the host's per-session execution queue. */
export async function restoreAgents(
  journal: FactJournal,
  state: AgentState,
  hooks: RestoreHooks,
) {
  await journal.initialize((event) => applyAgentEvent(state, event));
  for (const session of state.sessions.values()) hooks.validateConfig(session.config);
  for (const message of state.messages.values()) await hooks.read<string>(message.body);
  for (const job of state.jobs.values()) {
    hooks.validateConfig(job.config);
    const transcript: Message[] = [];
    for (const body of job.transcript) transcript.push(await hooks.read<Message>(body));
    transcriptCursor(transcript);
  }
  // Keep bodies on demand: hold at most one job's transcript, not the full history.
  for (const job of state.jobs.values()) {
    if (job.replySeq) continue;
    const transcript: Message[] = [];
    for (const body of job.transcript) transcript.push(await hooks.read<Message>(body));
    const cursor = transcriptCursor(transcript);
    if (job.cancelled) {
      await hooks.finish(
        job,
        "cancelled",
        cursor.assistant && !cursor.assistant.toolCalls?.length
          ? cursor.assistant.content
          : job.transcript.length
          ? "任务已取消，已执行动作与原始回执保留。"
          : "任务已取消，未开始后续工作。",
      );
    } else if (cursor.assistant && !cursor.assistant.toolCalls?.length) {
      if (hooks.canFinish?.(job, transcript) !== false) {
        await hooks.finish(job, "completed", cursor.assistant.content);
      }
    } else if (!cursor.assistant || cursor.completed === cursor.assistant.toolCalls?.length) {
      const saved = hooks.result?.(`${job.jobId}_model_${cursor.step + 1}`);
      if (saved && !saved.toolCalls.length) {
        await hooks.step(job, assistantMessage(saved));
        if (hooks.canFinish?.(job, [...transcript, assistantMessage(saved)]) !== false) {
          await hooks.finish(job, "completed", saved.content);
        }
      }
    }
  }
}
