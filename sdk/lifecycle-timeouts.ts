import { KernelError } from "../kernel/protocol.ts";

// A current model/tool may consume its entire configured deadline. Keep 30s for
// its durable receipt; dependency shutdown and control transport get another 30s.
export function eveStopDeadlineMs(modelTimeoutMs: number): number {
  if (
    !Number.isSafeInteger(modelTimeoutMs) || modelTimeoutMs < 1 ||
    modelTimeoutMs > 2_147_483_647 - 60_000
  ) throw new KernelError("rejected", "模型期限不能超出停止及控制计时器支持范围");
  return modelTimeoutMs + 30_000;
}

export function instanceStopControlDeadlineMs(modelTimeoutMs: number): number {
  return Math.max(180_000, eveStopDeadlineMs(modelTimeoutMs) + 30_000);
}
