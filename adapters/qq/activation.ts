// 首次上线用的通知游标边界，不改变真正的手机阅读状态，不搬运任何聊天正文。
import { QqPhone } from "./phone.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

export interface PhoneActivationBoundary {
  schema: 1;
  capturedAt: string;
  throughSeq: number;
  count: number;
  notificationCount: number;
  /** 仅seq/key/conversationId/notify的指纹，不包含正文、附件或身份私有数据。 */
  digest: string;
}

export async function ensurePhoneActivationBoundary(
  phone: QqPhone,
  directory: string,
): Promise<PhoneActivationBoundary> {
  const path = `${directory}/activation-boundary.json`;
  for await (const entry of Deno.readDir(directory)) {
    if (entry.name.startsWith("activation-boundary.json.") && entry.name.endsWith(".tmp")) {
      throw new KernelError("corrupt", "手机启动边界存在未完成提交，原件保留");
    }
  }
  try {
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink || stat.size > 4096) {
      throw new KernelError("corrupt", "手机启动边界异常，原件保留");
    }
    const saved = JSON.parse(await Deno.readTextFile(path)) as PhoneActivationBoundary;
    if (
      !saved || saved.schema !== 1 || !Number.isFinite(Date.parse(saved.capturedAt)) ||
      !Number.isSafeInteger(saved.throughSeq) || saved.throughSeq < 0 ||
      !Number.isSafeInteger(saved.count) || saved.count < 0 ||
      !Number.isSafeInteger(saved.notificationCount) || saved.notificationCount < 0 ||
      saved.notificationCount > saved.count || !/^[a-f0-9]{64}$/.test(saved.digest)
    ) {
      throw new KernelError("corrupt", "手机启动边界格式异常，原件保留");
    }
    const actual = await phone.captureNotificationBoundary(saved.throughSeq);
    if (
      actual.count !== saved.count || actual.notificationCount !== saved.notificationCount ||
      actual.digest !== saved.digest
    ) {
      throw new KernelError("corrupt", "手机启动边界与事实来源不一致，原件保留");
    }
    return saved;
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("corrupt", "手机启动边界无法恢复，原件保留");
    }
  }
  const captured: PhoneActivationBoundary = {
    schema: 1,
    capturedAt: new Date().toISOString(),
    ...await phone.captureNotificationBoundary(),
  };
  await writeDurableJson(path, captured);
  return captured;
}
