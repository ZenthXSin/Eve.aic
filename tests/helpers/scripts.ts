import assert from "node:assert/strict";
import type { ScriptSummary } from "../../contracts/script.ts";
import type { Kernel } from "../../kernel/kernel.ts";
import type { Json } from "../../kernel/protocol.ts";

/** Sync admission is bounded at five seconds; assert effects only after the original job settles. */
export async function settleScript(kernel: Kernel, job: ScriptSummary): Promise<ScriptSummary> {
  const deadline = performance.now() + 60_000;
  while (["queued", "checking", "running"].includes(job.status)) {
    assert.ok(performance.now() < deadline, JSON.stringify(job));
    await new Promise((done) => setTimeout(done, 25));
    job = await kernel.call<ScriptSummary & Json>("scripts", "status", { jobId: job.jobId });
  }
  return job;
}
