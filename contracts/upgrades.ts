/** 源码固定提交的升级请求由独立监督接收；脚本不获得守护或监督管理权限。 */
export const UPGRADES = "upgrades";
export const UPGRADE_METHODS = ["request", "status"] as const;

export type UpgradeFailure = {
  phase: string;
  operation: string;
  kind: string;
  message: string;
};

export interface UpgradeReceipt {
  actionId: string;
  /** 旧回执未记录来源时按 instance 理解，管理员申请始终明确标注。 */
  requestedBy?: "instance" | "administrator";
  commit: string;
  state: "preparing" | "submitted" | "failed";
  instanceId?: string;
  outcome?: string;
  upgradeId?: string;
  phase?: string;
  operation?: string;
  reason?: string;
  failure?: UpgradeFailure;
  error?: { kind: string; message: string };
}

export interface UpgradeApi {
  /** 仅接受已提交的完整 40 位 Git 提交；实际校验、切换和旧 Eve 确认由独立监督完成。 */
  request(args: { commit: string }): Promise<UpgradeReceipt>;
  /** 无参数查询最近一次申请；也可带原编号查询，不制造新申请。 */
  status(args?: { actionId?: string }): Promise<UpgradeReceipt>;
}
