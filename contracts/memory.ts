export interface MemorySource {
  recordSeq: number;
  digest: string;
  kind: "experienced" | "heard" | "imagined";
}
export interface MemoryView {
  /** 形成当时的意识原件水位；未确认候选的原件仍可查看，但不参与召回和强化。 */
  contextRecordSeq?: number;
  quarantined?: true;
  unconfirmedRecordSeqs?: number[];
  id: string;
  text: string;
  sources: MemorySource[];
  entities: string[];
  formedAt: string;
  level: number;
  strength: number;
}
export interface MemoryApi {
  /** 由你自己整理，第一人称；来源必须指向实际存在的意识记录。原件保留。 */
  remember(
    args: { text: string; sources: MemorySource[]; entities?: string[] },
  ): Promise<MemoryView>;
  recall(
    args: { query: string; limit?: number },
  ): Promise<{
    items: (MemoryView & { score: number })[];
    /** 语义模型接入 TODO；当前未配置模型时使用词法召回。 */
    retrieval: "lexical" | "semantic";
  }>;
  used(args: { ids: string[] }): Promise<{ reinforced: string[] }>;
  list(
    args?: { afterSeq?: number },
  ): Promise<{ items: MemoryView[]; throughSeq: number; hasMore: boolean }>;
  get(args: { id: string }): Promise<MemoryView>;
}
export interface RecordApi {
  /** 按含义/关键词搜索便签、聊天、子代理与意识历史；后台索引进度随结果返回。
   * 用 source.collection 选择 notes.get / phone.message / agents.message / records.text 展开原件。
   * 新记录尚未索引时，结果不代表完整历史；可用 indexStatus 查看进度。 */
  search(args: import("./search.ts").SearchQuery): Promise<import("./search.ts").SearchResult>;
  indexStatus(): Promise<import("./search.ts").SearchStatus>;
  page(
    args?: { afterSeq?: number },
  ): Promise<
    {
      items: { seq: number; at: string; kind: string; digest: string }[];
      throughSeq: number;
      hasMore: boolean;
    }
  >;
  text(
    args: { seq: number; startChar?: number; limit?: number },
  ): Promise<{ text: string; characters: number; nextChar?: number; seq: number; digest: string }>;
}
