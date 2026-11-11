import type { Json, WireError } from "../kernel/protocol.ts";

export type FromScript =
  | { t: "invoke"; seq: number; namespace: string; method: string; args: Json }
  | { t: "finished"; value: Json }
  | { t: "failed"; error: WireError };
export type ToScript =
  | { t: "start"; moduleUrl: string }
  | { t: "reply"; seq: number; ok: true; value: Json }
  | { t: "reply"; seq: number; ok: false; error: WireError };
