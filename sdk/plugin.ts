// 插件作者面对的接口。插件只依赖这里和 contracts/ 里的契约，不依赖内核实现。

import type { Json, Manifest } from "../kernel/protocol.ts";

export type { Json, Manifest };

export type Handler = (args: Json) => Json | Promise<Json>;

export interface CallOptions {
  timeoutMs?: number;
  /** 同一个键的调用只执行一次，重发时返回第一次的结果。 */
  idempotencyKey?: string;
}

export interface PluginContext {
  readonly manifest: Manifest;
  /** 提供一个契约的实现；只能在 load 阶段调用。 */
  provide(contract: string, handlers: Record<string, Handler>): void;
  /** 调用别的插件提供的契约；必须在清单的 requires 或 optional 里声明过。 */
  call<T extends Json = Json>(
    contract: string,
    method: string,
    args?: Json,
    options?: CallOptions,
  ): Promise<T>;
  /** 读取自己的状态；没有时返回 undefined。 */
  getState<T extends Json = Json>(): T | undefined;
  /** 写入自己的状态；内核负责落盘、快照与回滚。 */
  setState(state: Json): void;
  /** 登记一个清理动作，卸载时按登记的逆序执行。 */
  onDisable(cleanup: () => void | Promise<void>): void;
  log(message: string): void;
}

export interface PluginModule {
  /** 只做定义：提供契约、登记处理器。不应产生运行时副作用。 */
  load?(ctx: PluginContext): void | Promise<void>;
  /** 真正启用：启动定时器、打开连接等。 */
  enable?(ctx: PluginContext): void | Promise<void>;
  /** 清理。onDisable 登记的动作会在这之后执行。 */
  disable?(ctx: PluginContext): void | Promise<void>;
  /** 健康探测；抛错表示不健康。 */
  health?(ctx: PluginContext): void | Promise<void>;
  /** 状态格式升级：把旧版本的状态转换成当前 stateVersion 的格式。 */
  migrate?(state: Json, fromStateVersion: number): Json | Promise<Json>;
}

export function definePlugin(module: PluginModule): PluginModule {
  return module;
}
