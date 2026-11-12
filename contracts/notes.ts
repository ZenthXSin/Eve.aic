export interface NoteSource {
  kind: "experienced" | "heard" | "imagined";
  at: string;
  description: string;
}
export interface NoteRevision {
  /** 意识水位及明确的版本前件，候选回滚不会合并其编辑。 */
  contextRecordSeq?: number;
  basedOnRevision?: number;
  quarantined?: true;
  unconfirmedRecordSeqs?: number[];
  id: string;
  /** 可选唯一短名，1–64 字符；不含空白、控制字符、斜线，也不能是完整 ID。 */
  alias?: string;
  revision: number;
  recordSeq: number;
  at: string;
  title: string;
  group: string;
  summary: string;
  content: string;
  sources: NoteSource[];
}
export interface NoteApi {
  create(
    args: {
      title: string;
      alias?: string;
      group?: string;
      summary?: string;
      content: string;
      sources: NoteSource[];
    },
  ): Promise<NoteRevision>;
  update(
    args: {
      /** 永久 ID 或当前短名。改名后旧短名不再指向这张便签。 */
      id: string;
      /** 省略时先读当前版本再做 CAS；基于先前读取内容编辑时应传入其 revision。 */
      expectedRevision?: number;
      /** 设置/改名；null 移除短名，省略则保留。 */
      alias?: string | null;
      title?: string;
      group?: string;
      summary?: string;
      content?: string;
      sources?: NoteSource[];
    },
  ): Promise<NoteRevision>;
  /** id 接受永久 ID 或当前短名。 */
  get(args: { id: string; revision?: number; startChar?: number; limit?: number }): Promise<{
    note: Omit<NoteRevision, "content">;
    text: string;
    characters: number;
    nextChar?: number;
  }>;
  list(
    args?: { group?: string; cursor?: { snapshotThroughSeq: number; afterSeq: number } },
  ): Promise<{
    items: Omit<NoteRevision, "content" | "sources">[];
    snapshotThroughSeq: number;
    older?: { snapshotThroughSeq: number; afterSeq: number };
  }>;
  versions(
    args: { id: string; afterRevision?: number },
  ): Promise<{ items: Omit<NoteRevision, "content">[]; hasMore: boolean }>;
  search(
    args: {
      query: string;
      mode?: "literal" | "regex" | "semantic";
      group?: string;
      afterSeq?: number;
    },
  ): Promise<{
    items: {
      id: string;
      alias?: string;
      revision: number;
      title: string;
      excerpt: string;
      score?: number;
    }[];
    throughSeq: number;
    hasMore: boolean;
  }>;
}
