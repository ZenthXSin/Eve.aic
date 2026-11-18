// 可独立运行的接收器；持有目录排他锁，先保留消息再提交网关水位。这里不调模型、不发消息。
import { QqClient } from "./client.ts";
import { FileGatewayCheckpoint } from "./checkpoint.ts";
import { QqGateway, type QqGatewayStatus } from "./gateway.ts";
import { type QqInboundEvent, qqMessage } from "./inbound.ts";
import { type InboundRecord, QqInboundJournal } from "./journal.ts";
import { QqPlatformEvents } from "./platform-events.ts";

export type QqReceiverStatus = QqGatewayStatus | {
  status: "webhook_listening" | "webhook_verified";
  code?: undefined;
  reason?: undefined;
  closeTimedOut?: undefined;
};

export interface QqIngressOptions {
  intents?: number;
  operatorInteractionTargets?: boolean;
  webhook?: (
    signal: AbortSignal,
    accept: (event: QqInboundEvent) => Promise<void>,
    listening: () => void,
  ) => Promise<void>;
  observed?: (event: QqInboundEvent, record?: InboundRecord) => Promise<void>;
}

export class QqReceiver {
  #running = false;
  #journal: QqInboundJournal;
  #platformEvents: QqPlatformEvents;
  constructor(
    readonly client: QqClient,
    readonly directory: string,
    readonly hooks: {
      initialized?: (records: InboundRecord[]) => Promise<void>;
      persisted?: (record: InboundRecord) => void;
    } = {},
    readonly ingress: QqIngressOptions = {},
  ) {
    this.#journal = new QqInboundJournal(`${directory}/inbound`);
    this.#platformEvents = new QqPlatformEvents(`${directory}/platform-events`, ingress);
  }

  /** 可供后续手机/通知按序查看；不改已读或动作状态。 */
  messages() {
    return this.#journal.records();
  }

  get journal() {
    return this.#journal;
  }

  get platformEvents() {
    return this.#platformEvents;
  }

  async run(signal: AbortSignal, onStatus?: (status: QqReceiverStatus) => void): Promise<void> {
    if (this.#running) throw new Error("QQ 接收器已经运行");
    this.#running = true;
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
      lock = await Deno.open(`${this.directory}/receiver.lock`, {
        create: true,
        read: true,
        write: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) throw new Error("已有 QQ 接收器持有此目录");
      this.#journal = new QqInboundJournal(`${this.directory}/inbound`);
      this.#platformEvents = new QqPlatformEvents(
        `${this.directory}/platform-events`,
        this.ingress,
      );
      await this.#journal.initialize();
      await this.#platformEvents.initialize();
      await this.hooks.initialized?.(this.#journal.records());
      const dispatch = async (event: QqInboundEvent) => {
        const message = qqMessage(event);
        const stored = message ? await this.#journal.append(message, event) : undefined;
        if (stored?.fresh) this.hooks.persisted?.(stored.record);
        if (event.t !== "READY" && event.t !== "RESUMED") {
          await this.#platformEvents.observe(event, stored?.record);
          await this.ingress.observed?.(event, stored?.record);
        }
      };
      if (this.ingress.webhook) {
        await this.ingress.webhook(signal, async (event) => {
          await dispatch(event);
          onStatus?.({ status: "webhook_verified" });
        }, () => onStatus?.({ status: "webhook_listening" }));
        return;
      }
      const intents = this.ingress.intents ?? (1 << 25);
      if (!Number.isSafeInteger(intents) || intents < 0 || intents > 0xffffffff) {
        throw new Error("QQ 网关订阅位无效");
      }
      const sessionName = intents === (1 << 25) ? "session" : `session-intents-${intents}`;
      const gateway = new QqGateway(this.client, {
        intents,
        checkpoint: new FileGatewayCheckpoint(`${this.directory}/gateway/${sessionName}.json`),
        onStatus,
        onDispatch: dispatch,
      });
      await gateway.run(signal);
    } finally {
      lock?.close();
      this.#running = false;
    }
  }
}
