import type { ScriptApi } from "../../contracts/script.ts";
import { PHONE_METHODS } from "../../contracts/phone-rpc.ts";
import { RUNTIME_METHODS } from "../../contracts/runtime.ts";
import { UPGRADE_METHODS } from "../../contracts/upgrades.ts";

/** Keys and each method are checked against the same ScriptApi consumed by Eve. */
export const SCRIPT_TOOL_METHODS = {
  web: RUNTIME_METHODS.web,
  upgrades: UPGRADE_METHODS,
  notes: RUNTIME_METHODS.notes,
  files: RUNTIME_METHODS.files,
  commands: RUNTIME_METHODS.commands,
  memory: RUNTIME_METHODS.memory,
  records: RUNTIME_METHODS.records,
  mind: ["rest", "remind", "watch", "cancelReminder", "compress", "retain", "status"],
  phone: PHONE_METHODS,
  scripts: ["status", "cancel", "result", "receipt"],
} as const satisfies { [N in keyof ScriptApi]: readonly (keyof ScriptApi[N])[] };
type MissingMethods = {
  [N in keyof ScriptApi]: Exclude<keyof ScriptApi[N], (typeof SCRIPT_TOOL_METHODS)[N][number]>;
}[keyof ScriptApi];
// Adding a public method without updating its visible catalog is a type-check error.
const complete: [MissingMethods] extends [never] ? true : never = true;
void complete;
export const TOOL_CATALOG = [
  {
    namespace: "native",
    methods: [
      "execute_script",
      "write_thought",
      "confirm_upgrade",
      "save_observation_note",
      "trial_candidate",
    ],
    conditionalMethods: ["confirm_upgrade", "save_observation_note", "trial_candidate"],
    context:
      "confirm_upgrade / save_observation_note / trial_candidate 仅升级观察上下文，常规在线不可调用",
  },
  ...Object.entries(SCRIPT_TOOL_METHODS).map(([namespace, methods]) => ({
    namespace,
    methods: [...methods],
    conditionalMethods: [] as string[],
    context: "",
  })),
];
