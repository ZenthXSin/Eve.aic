/** 源码固定提交的升级请求由独立监督接收；脚本不获得守护或监督管理权限。 */
export const UPGRADES = "upgrades";
export const UPGRADE_METHODS = ["request", "status"] as const;

export interface UpgradeReceipt {
  actionId: string;
  commit: string;
  state: "preparing" | "submitted" | "failed";
  instanceId?: string;
  outcome?: string;
  error?: { kind: string; message: string };
}

export interface UpgradeApi {
  /** 仅接受已提交的完整 40 位 Git 提交；实际校验、切换和旧 Eve 确认由独立监督完成。 */
  request(args: { commit: string }): Promise<UpgradeReceipt>;
  /** 查询原升级申请编号，不制造新的升级请求。 */
  status(args: { actionId: string }): Promise<UpgradeReceipt>;
}
