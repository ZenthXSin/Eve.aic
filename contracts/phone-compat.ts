/** Runtime type-check compatibility only; deprecated arguments are not advertised to Eve. */
export interface LegacyReply {
  replyTo?: { seq: number; sequence?: number };
}
