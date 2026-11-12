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
      group?: string;
      summary?: string;
      content: string;
      sources: NoteSource[];
    },
  ): Promise<NoteRevision>;
  update(
    args: {
      id: string;
      expectedRevision: number;
      title?: string;
      group?: string;
      summary?: string;
      content?: string;
      sources?: NoteSource[];
    },
  ): Promise<NoteRevision>;
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
    items: { id: string; revision: number; title: string; excerpt: string; score?: number }[];
    throughSeq: number;
    hasMore: boolean;
  }>;
}
