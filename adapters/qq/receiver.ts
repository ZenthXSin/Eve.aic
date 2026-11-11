// 可独立运行的接收器；持有目录排他锁，先保留消息再提交网关水位。这里不调模型、不发消息。
import { QqClient } from "./client.ts";
import { FileGatewayCheckpoint } from "./checkpoint.ts";
import { QqGateway } from "./gateway.ts";
import { qqMessage } from "./inbound.ts";
import { QqInboundJournal } from "./journal.ts";

export interface QqReceiverStatus {
  status: "connecting" | "connected" | "hello" | "ready" | "resumed" | "disconnected";
  code?: number;
}

export class QqReceiver {
  #running = false;
  #journal: QqInboundJournal;
  constructor(readonly client: QqClient, readonly directory: string) {
    this.#journal = new QqInboundJournal(`${directory}/inbound`);
  }

  /** 可供后续手机/通知按序查看；不改已读或动作状态。 */
  messages() {
    return this.#journal.records();
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
      await this.#journal.initialize();
      const gateway = new QqGateway(this.client, {
        intents: 1 << 25,
        checkpoint: new FileGatewayCheckpoint(`${this.directory}/gateway/session.json`),
        onStatus,
        onDispatch: async (event) => {
          const message = qqMessage(event);
          if (message) await this.#journal.append(message, event);
        },
      });
      await gateway.run(signal);
    } finally {
      lock?.close();
      this.#running = false;
    }
  }
}
