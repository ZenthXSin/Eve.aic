export interface FileApi {
  read(
    args: { path: string; offsetBytes?: number; limitBytes?: number; encoding?: "utf8" | "base64" },
  ): Promise<{
    path: string;
    sha256: string;
    sizeBytes: number;
    offsetBytes: number;
    text: string;
    nextOffsetBytes?: number;
  }>;
  write(
    args: { path: string; text: string; expectedSha256: string | "absent" },
  ): Promise<{ path: string; sha256: string; sizeBytes: number }>;
  replace(
    args: { path: string; oldText: string; newText: string; expectedSha256: string },
  ): Promise<{ path: string; sha256: string; sizeBytes: number }>;
  list(
    args: { path?: string; afterName?: string },
  ): Promise<
    { items: { name: string; kind: "file" | "directory" | "symlink" }[]; hasMore: boolean }
  >;
  search(
    args: { path: string; query: string; afterLine?: number },
  ): Promise<{ items: { line: number; text: string }[]; throughLine: number; hasMore: boolean }>;
}
export interface CommandView {
  sessionId: string;
  state: "running" | "completed" | "unknown";
  exitCode?: number;
  stdout: { text: string; nextOffsetBytes: number; sizeBytes: number; missingOriginal?: boolean };
  stderr: { text: string; nextOffsetBytes: number; sizeBytes: number; missingOriginal?: boolean };
}
export interface CommandApi {
  /** 持久完成事实；只提示实际终止或崩溃后结果未知，游标跨重启继续。 */
  completions(
    args?: { afterSeq?: number },
  ): Promise<{ items: CommandCompletion[]; throughSeq: number; hasMore: boolean }>;
  start(args: { cmd: string; cwd?: string; waitMs?: number }): Promise<CommandView>;
  poll(
    args: {
      sessionId: string;
      stdoutOffsetBytes?: number;
      stderrOffsetBytes?: number;
      limitBytes?: number;
      waitMs?: number;
    },
  ): Promise<CommandView>;
  write(args: { sessionId: string; text?: string; closeStdin?: boolean }): Promise<CommandView>;
  cancel(args: { sessionId: string }): Promise<CommandView>;
}
export interface CommandCompletion {
  seq: number;
  at: string;
  sessionId: string;
  requestId: string;
  state: "completed" | "unknown";
  exitCode?: number;
}
