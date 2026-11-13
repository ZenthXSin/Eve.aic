/** 源码固定提交的升级请求由独立监督接收；脚本不获得守护或监督管理权限。 */
export const UPGRADE_DIRECT_REPLACEMENT_VERSION = 1;
export const UPGRADES = "upgrades";
export const UPGRADE_METHODS = ["request", "status", "logs"] as const;

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
  /** 仅接受已提交的完整40位Git提交；监督校验后停旧程序，从原数据目录直接启动新版，核验健康。 */
  request(args: { commit: string }): Promise<UpgradeReceipt>;
  /** 无参数查询最近一次申请；也可带原编号查询，不制造新申请。 */
  status(args?: { actionId?: string }): Promise<UpgradeReceipt>;
  /** 不指定source列出全部日志源；文件按字节分页（base64无损），系统日志按cursor分页。
   * 不截断日志总量。文件limit=1..262144字节，journal limit=1..1000条。
   * 文件：从offset=0循环nextOffset直到eof，拼接base64解码后的字节再UTF-8解码。
   * 系统日志：首次省略cursor，从最早留存记录开始，后续传nextCursor；eof后仍可轮询新增。
   * 包括升级检查/测试全文、实例输出、QQ连接分阶段诊断及五个宿主服务的全部留存日志。
   */
  logs(args?: { source?: string; offset?: number; cursor?: string; limit?: number }): Promise<{
    sources?: { id: string; kind: "file" | "journal"; bytes?: number }[];
    encoding?: "base64";
    data?: string;
    offset?: number;
    nextOffset?: number;
    totalBytes?: number;
    items?: import("../kernel/protocol.ts").Json[];
    nextCursor?: string;
    eof?: boolean;
  }>;
}
