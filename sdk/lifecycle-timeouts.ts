import { MODEL_RPC_TIMEOUT_MS } from "./model-timeouts.ts";
import { KernelError } from "../kernel/protocol.ts";

// Scripts and Eve restore in sequence, with separate 5-minute enable windows.
// The transport must cover both plus composition/loading, or a supervisor timeout
// revokes the lease while the instance is still legitimately restoring.
export function instanceStartControlDeadlineMs(startupTimeoutMs = 300_000): number {
  if (
    !Number.isSafeInteger(startupTimeoutMs) || startupTimeoutMs < 1 ||
    startupTimeoutMs > 2_147_483_647 - 360_000
  ) throw new KernelError("rejected", "启动期限不能超出恢复及控制计时器支持范围");
  return startupTimeoutMs + 360_000;
}

// A streaming model can outlive the legacy total deadline. Let its bounded model
// RPC finish, then retain 30s for shutdown; the control transport gets another 30s.
export function eveStopDeadlineMs(modelTimeoutMs: number): number {
  if (
    !Number.isSafeInteger(modelTimeoutMs) || modelTimeoutMs < 1 ||
    modelTimeoutMs > 2_147_483_647 - 60_000
  ) throw new KernelError("rejected", "模型期限不能超出停止及控制计时器支持范围");
  return Math.max(modelTimeoutMs, MODEL_RPC_TIMEOUT_MS) + 30_000;
}

export function instanceStopControlDeadlineMs(modelTimeoutMs: number): number {
  return Math.max(180_000, eveStopDeadlineMs(modelTimeoutMs) + 30_000);
}
