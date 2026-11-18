import { KernelError } from "../../kernel/protocol.ts";

export type StudioValue = boolean | number | string;
export type StudioFileScope = "supervisor" | "instance" | "runtime" | "qq" | "guardian" | "admin";
export type StudioScope = StudioFileScope | "budget" | "pause";
export type StudioItem =
  & {
    key: string;
    label: string;
    group: string;
    scope: StudioScope;
    mode: "hot" | "restart" | "restricted";
    editable: boolean;
    description: string;
    reason?: string;
  }
  & (
    | { type: "secret"; isSet: boolean; value?: never }
    | {
      type: "boolean" | "number" | "enum" | "string";
      value: StudioValue | null;
      min?: number;
      max?: number;
      options?: string[];
    }
  );
export type StudioSnapshot = { revision: string; items: StudioItem[] };
export type StudioUpdate = { revision: string; changes: Record<string, StudioValue> };

const failures = {
  invalid: [400, "rejected", "设置请求无效，请检查字段、类型和取值范围"],
  scope: [400, "rejected", "一次只能保存同一持久来源的设置"],
  restricted: [403, "rejected", "此设置受限或当前不可编辑"],
  conflict: [409, "rejected", "设置版本已变化，请重新读取后核对修改"],
  unavailable: [503, "unavailable", "无法安全读取设置来源，请检查服务与部署配置"],
  unconfirmed: [503, "unknown", "设置结果未确认，请刷新核对，不要自动重发"],
} as const;

export class StudioSettingsError extends KernelError {
  constructor(readonly code: keyof typeof failures) {
    super(failures[code][1], failures[code][2]);
  }
}

export function studioSettingsFailure(error: unknown) {
  const code =
    (Object.keys(failures) as (keyof typeof failures)[]).find((key) =>
      error instanceof KernelError && error.kind === failures[key][1] &&
      error.message === failures[key][2]
    ) ?? "unconfirmed";
  return { status: failures[code][0], error: { code, message: failures[code][2] } };
}

export function studioRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" &&
    [Object.prototype, null].includes(Object.getPrototypeOf(value));
}

export function validateStudioUpdate(body: unknown): StudioUpdate {
  if (
    !studioRecord(body) || Object.keys(body).length !== 2 || !Object.hasOwn(body, "revision") ||
    !Object.hasOwn(body, "changes") || typeof body.revision !== "string" ||
    !/^[a-f0-9]{64}$/.test(body.revision) || !studioRecord(body.changes)
  ) throw new StudioSettingsError("invalid");
  const entries = Object.entries(body.changes);
  if (entries.length < 1 || entries.length > 32) throw new StudioSettingsError("invalid");
  const changes: Record<string, StudioValue> = Object.create(null);
  for (const [key, value] of entries) {
    if (
      key.length > 80 || !/^[a-z][a-zA-Z0-9]*(?:\.[a-zA-Z][a-zA-Z0-9]*)+$/.test(key) ||
      key.split(".").some((part) => ["constructor", "prototype"].includes(part)) ||
      !(typeof value === "boolean" || (typeof value === "number" && Number.isSafeInteger(value)) ||
        (typeof value === "string" && value.length > 0 && value.length <= 128 &&
          Array.from(value).every((char) =>
            char.charCodeAt(0) >= 32 && char.charCodeAt(0) !== 127
          )))
    ) throw new StudioSettingsError("invalid");
    changes[key] = value;
  }
  return { revision: body.revision, changes };
}
