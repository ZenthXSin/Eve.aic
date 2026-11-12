import type { PluginStopFailure } from "../kernel/kernel.ts";
import { type ErrorKind, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

export interface StopReport {
  schema: 1;
  at: string;
  instanceId: string;
  sourceCommit: string;
  failures: PluginStopFailure[];
  persisted: boolean;
  persistenceError?: string;
}

export function stopErrorKind(failures: PluginStopFailure[]): ErrorKind {
  // A real persistence failure always takes precedence over a concurrent timeout.
  for (const kind of ["storage", "corrupt", "timeout"] as const) {
    if (failures.some((failure) => failure.kind === kind)) return kind;
  }
  return failures[0]?.kind ?? "unknown";
}

/** Immutable failure reports survive successful retries, activation and restarts. */
export class StopDiagnostics {
  last: StopReport | null = null;
  constructor(readonly directory: string) {}

  async initialize() {
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    const names: string[] = [];
    for await (const entry of Deno.readDir(this.directory)) {
      if (entry.isFile && /^\d{13}-[a-f0-9-]+\.json$/.test(entry.name)) names.push(entry.name);
    }
    const latest = names.sort().at(-1);
    if (!latest) return;
    try {
      const value = JSON.parse(await Deno.readTextFile(`${this.directory}/${latest}`));
      if (value.schema !== 1 || value.persisted !== true || !Array.isArray(value.failures)) {
        throw new Error("停止诊断格式不完整");
      }
      this.last = value;
    } catch {
      throw new KernelError("corrupt", "停止诊断原件无法读取，已保留");
    }
  }

  async record(instanceId: string, sourceCommit: string, failures: PluginStopFailure[]) {
    const report: StopReport = {
      schema: 1,
      at: new Date().toISOString(),
      instanceId,
      sourceCommit,
      failures: structuredClone(failures),
      persisted: false,
    };
    this.last = report;
    try {
      await writeDurableJson(
        `${this.directory}/${Date.now()}-${crypto.randomUUID()}.json`,
        { ...report, persisted: true },
      );
      report.persisted = true;
    } catch (error) {
      report.persistenceError = error instanceof Error ? error.message : String(error);
      throw new KernelError("storage", "停止诊断未能持久保存，原停止错误仍保留在状态中");
    }
  }
}
