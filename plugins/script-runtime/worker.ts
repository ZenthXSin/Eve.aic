/// <reference lib="deno.worker" />
// 每次脚本自己的 worker；嵌套动作只通过已声明的类型接口转发到宿主。
import type { ScriptApi } from "../../contracts/script.ts";
import type { FromScript, ToScript } from "../../contracts/script-wire.ts";
import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { PHONE_METHODS } from "../../contracts/phone-rpc.ts";
import { RUNTIME_METHODS } from "../../contracts/runtime.ts";
import { UPGRADE_METHODS } from "../../contracts/upgrades.ts";

let seq = 0;
const pending = new Map<number, { resolve(value: Json): void; reject(error: Error): void }>();
function send(message: FromScript) {
  self.postMessage(message);
}
function invoke(namespace: string, method: string, args: Json): Promise<Json> {
  const index = ++seq;
  return new Promise((resolve, reject) => {
    pending.set(index, { resolve, reject });
    send({ t: "invoke", seq: index, namespace, method, args });
  });
}
const phone = Object.fromEntries(PHONE_METHODS.map((method) => [
  method,
  (args: Json = {}) => invoke("phone", method, args),
]));
const scripts = Object.fromEntries(["status", "cancel", "result", "receipt"].map((method) => [
  method,
  (args: Json) => invoke("scripts", method, args),
]));
const mind = Object.fromEntries(
  ["rest", "remind", "watch", "cancelReminder", "compress", "retain", "status"].map((method) => [
    method,
    (args: Json = {}) => invoke("mind", method, args),
  ]),
);
const tools = Object.freeze({
  upgrades: Object.freeze(Object.fromEntries(UPGRADE_METHODS.map((method) => [
    method,
    (args: Json = {}) => invoke("upgrades", method, args),
  ]))),
  ...Object.fromEntries(
    ["notes", "files", "commands", "memory", "records"].map((
      namespace,
    ) => [
      namespace,
      Object.freeze(
        Object.fromEntries(
          RUNTIME_METHODS[namespace as "notes"].map((
            method,
          ) => [method, (args: Json = {}) => invoke(namespace, method, args)]),
        ),
      ),
    ]),
  ),
  phone: Object.freeze(phone),
  scripts: Object.freeze(scripts),
  mind: Object.freeze(mind),
});
let started = false;
self.onmessage = async (event: MessageEvent<ToScript>) => {
  const message = event.data;
  if (message.t === "reply") {
    const waiter = pending.get(message.seq);
    if (!waiter) return;
    pending.delete(message.seq);
    if (message.ok) waiter.resolve(message.value);
    else waiter.reject(new KernelError(message.error.kind, message.error.message));
    return;
  }
  if (started) return;
  started = true;
  try {
    const module = await import(message.moduleUrl);
    const result = await module.default(tools as unknown as ScriptApi);
    // 未 await 的动作也必须等实际回执；脚本返回不取消已完成或正在保存的动作。
    while (pending.size) await new Promise((done) => setTimeout(done, 1));
    const value = result === undefined ? null : JSON.parse(JSON.stringify(result));
    send({ t: "finished", value });
  } catch (error) {
    send({ t: "failed", error: toWireError(error) });
  }
};
