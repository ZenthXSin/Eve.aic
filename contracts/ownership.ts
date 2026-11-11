/** 守护签发的递增代际与实例凭据。凭据只在本机IPC中传递，不进入业务回执。 */
export interface OwnershipProof {
  instanceId: string;
  epoch: number;
  credential: string;
}
export interface OwnershipOwner {
  instanceId: string;
  epoch: number;
  expiresAt: number;
}
export interface OwnershipSnapshot {
  epoch: number;
  mode: "idle" | "active" | "draining" | "paused";
  owner: OwnershipOwner | null;
  activePermits: number;
  pendingRequest: string | null;
}
export interface OwnershipGrant {
  instanceId: string;
  requestId: string;
  expectedEpoch: number;
  /** 仅主人的显式恢复路径可设置；自动接管不能绕过已持久化的暂停。 */
  resumePaused?: boolean;
}
export interface OwnershipPermit {
  id: string;
  epoch: number;
  instanceId: string;
  resource: string;
  actionId: string;
}
export type PermitOutcome = "completed" | "failed" | "unknown" | "not_executed";
export interface ExitPermitRequest {
  /** 出口先存盘的申请编号；回执丢失后仍能核对同一次申请。 */
  permitId: string;
  proof: OwnershipProof;
  resource: string;
  actionId: string;
  /** 参数摘要，不向守护复制消息、脚本或私人正文。 */
  requestHash: string;
}
/** 出口拿到许可后等待真实执行终结，而非调用方等待超时；随后提交终态。 */
export interface OwnershipExit {
  begin(request: ExitPermitRequest): Promise<OwnershipPermit>;
  /** 仅在出口持久记录能证明真实操作尚未开始时调用；同时阻止迟到的 begin。 */
  cancel(permit: OwnershipPermit, requestHash: string): Promise<void>;
  finish(id: string, outcome: PermitOutcome, resultHash?: string): Promise<void>;
}
