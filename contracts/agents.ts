/** 子代理是独立持久会话，不进入 QQ 平台；整条消息保存，不自动拆分。 */
export type AgentRole = "review" | "coding" | "investigation";
export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh";
export interface AgentConfig {
  role: AgentRole;
  name: string;
  /** 省略则跟随共享模型选择；显式指定仅作用于此实例。 */
  model?: string;
  reasoningEffort: ReasoningEffort;
  instructions: string;
  /** 上游单次生成上限，不是消息字数上限，不自动拆成多条。 */
  maxOutputTokens?: number;
}
export interface AgentView {
  agentId: string;
  config: AgentConfig;
  state: "idle" | "working" | "closed";
  unread: number;
  pending: number;
  latest?: AgentMessagePreview;
}
export interface AgentMessagePreview {
  seq: number;
  agentId: string;
  jobId: string;
  direction: "incoming" | "outgoing";
  at: string;
  preview: string;
  characters: number;
  truncated: boolean;
  status: "queued" | "running" | "completed" | "failed" | "unknown" | "cancelled";
}
export interface AgentJob {
  jobId: string;
  agentId: string;
  status: AgentMessagePreview["status"];
  requestSeq: number;
  replySeq?: number;
  error?: { kind: string; message: string };
}
export interface AgentApi {
  /** 创建任意多个同类/不同类实例；默认思考强度 high。 */
  create(
    args: {
      role: AgentRole;
      name?: string;
      model?: string;
      reasoningEffort?: ReasoningEffort;
      instructions?: string;
      maxOutputTokens?: number;
    },
  ): Promise<AgentView>;
  configure(
    args: {
      agentId: string;
      name?: string;
      model?: string | null;
      reasoningEffort?: ReasoningEffort;
      instructions?: string;
      maxOutputTokens?: number | null;
    },
  ): Promise<AgentView>;
  list(
    args?: { afterId?: string; limit?: number },
  ): Promise<{ items: AgentView[]; next?: { afterId: string; limit: number } }>;
  /** 仅等待持久准入，后台工作；同实例串行，不同实例并行。 */
  send(args: { agentId: string; text: string }): Promise<AgentJob>;
  status(args: { jobId: string }): Promise<AgentJob>;
  /** 点开后更新已读水位，不把历史全文塞进主意识。 */
  open(
    args: { agentId: string },
  ): Promise<{ agent: AgentView; messages: AgentMessagePreview[]; older?: { beforeSeq: number } }>;
  history(
    args: { agentId: string; beforeSeq?: number; limit?: number },
  ): Promise<{ messages: AgentMessagePreview[]; older?: { beforeSeq: number } }>;
  /** 默认返回完整原消息；可选择分页读取，仍是一条消息。 */
  message(
    args: { seq: number; startChar?: number; limit?: number },
  ): Promise<AgentMessagePreview & { text: string; startChar: number; nextChar?: number }>;
  search(
    args: { query: string; agentId?: string; beforeSeq?: number; limit?: number },
  ): Promise<{ messages: AgentMessagePreview[]; older?: { beforeSeq: number } }>;
  notifications(
    args?: { afterSeq?: number },
  ): Promise<{ items: AgentMessagePreview[]; throughSeq: number; hasMore: boolean }>;
  /** 停止后续模型/工具步骤；已开始的动作保留真实回执，不伪装撤销。 */
  cancel(args: { jobId: string }): Promise<AgentJob>;
  close(args: { agentId: string }): Promise<AgentView>;
}
