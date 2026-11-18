import type { Json } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";

export interface ContinuityJob {
  status: string;
  observation?: { record: number; recordedAt: string; source: string };
}
const statuses = ["queued", "checking", "running", "completed", "failed", "cancelled", "unknown"];
const terminal = ["completed", "failed", "cancelled"];
function object(value: Json | undefined): Record<string, Json> | undefined {
  return value && typeof value === "object" && !Array.isArray(value) ? value : undefined;
}

/** Presentation only: consume saved receipts, never query jobs or drive wake/execution state. */
export class JobObservations {
  readonly jobs = new Map<string, ContinuityJob>();

  #remember(value: Json, event: ConsciousEvent, source: string, returnedAgent = false) {
    const job = object(value);
    if (
      !job || typeof job.jobId !== "string" || typeof job.status !== "string" ||
      !statuses.includes(job.status)
    ) return;
    const previous = this.jobs.get(job.jobId);
    // Script text is user output. Only a complete AgentJob-shaped return for an already
    // tracked job is useful here; do not recursively hunt for statuses or invent children.
    if (
      returnedAgent && (!previous || typeof job.agentId !== "string" || !job.agentId ||
        !Number.isSafeInteger(job.requestSeq) || Number(job.requestSeq) < 1 ||
        job.status === "checking" ||
        job.childJobIds !== undefined && (!Array.isArray(job.childJobIds) ||
            !job.childJobIds.every((id) => typeof id === "string" && !!id)))
    ) return;
    const status = job.status;
    // Preserve the existing terminal/unknown retirement contract. A late admission
    // receipt also cannot undo a saved observation that this job has started.
    if (
      previous && (terminal.includes(previous.status) ||
        previous.status === "unknown" && !terminal.includes(status) ||
        previous.status === "running" && ["queued", "checking"].includes(status))
    ) return;
    this.jobs.set(job.jobId, {
      status,
      observation: { record: event.seq, recordedAt: event.at, source },
    });
  }

  observe(event: ConsciousEvent) {
    const data = object(event.data);
    if (!data) return;
    if (event.kind === "notice" && (data.source === "agents" || data.source === "scripts")) {
      if (Array.isArray(data.items)) {
        for (const item of data.items) {
          this.#remember(object(item)?.job ?? item, event, `${data.source}_notification`);
        }
      }
    }
    if (event.kind !== "tool_result") return;
    const result = object(data.result);
    if (result?.status !== "completed" || typeof result.output !== "string") return;
    let summary: Record<string, Json> | undefined;
    try {
      summary = object(JSON.parse(result.output));
    } catch { /* Not a structured job receipt. */ }
    if (!summary) return;
    this.#remember(summary, event, "tool_receipt");
    const page = object(summary.result);
    // Only the documented completed script envelope and a whole first result page.
    // Missing/truncated/unknown output is not evidence of task progress.
    if (
      summary.status !== "completed" || summary.scriptStarted !== true ||
      !page || page.truncated !== false || page.nextChar !== undefined ||
      summary.resultStartChar !== undefined && summary.resultStartChar !== 0 ||
      typeof page.text !== "string" || page.characters !== Array.from(page.text).length
    ) return;
    try {
      this.#remember(JSON.parse(page.text), event, "script_result", true);
    } catch { /* Arbitrary script output is not a job observation. */ }
  }
}
