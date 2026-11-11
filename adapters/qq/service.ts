import { QqSendActions } from "./actions.ts";
import { QqClient } from "./client.ts";
import { QqPhone } from "./phone.ts";
import { QqReceiver, type QqReceiverStatus } from "./receiver.ts";
import { startPhoneRpc } from "./rpc.ts";
import { QqTimeline } from "./timeline.ts";

/** 一份适配器进程的生命周期，模型和手机插件不在此进程运行。 */
export async function startQqPhoneAdapter(
  client: QqClient,
  directory: string,
  key: string,
  onStatus?: (status: QqReceiverStatus) => void,
) {
  const controller = new AbortController();
  const actions = new QqSendActions(client, `${directory}/outbox`);
  const timeline = new QqTimeline(`${directory}/timeline`);
  const receiver = new QqReceiver(client, directory);
  let rpc: ReturnType<typeof startPhoneRpc> | undefined;
  let failure: unknown;
  let status: QqReceiverStatus = { status: "connecting" };
  let initialized!: () => void;
  const readyForQuery = new Promise<void>((resolve) => {
    initialized = resolve;
  });
  let receive: Promise<void> | undefined;
  try {
    await actions.initialize();
    await timeline.initialize();
    const self = await client.botIdentity();
    receive = receiver.run(controller.signal, (event) => {
      status = event;
      // 网关开始连接前，入站日志已恢复且目录锁已拿到；不是平台 Ready。
      if (event.status === "connecting") initialized();
      onStatus?.(event);
    });
    receive.catch((error) => {
      failure = error;
      controller.abort();
    });
    await Promise.race([
      readyForQuery,
      receive.then(() => {
        throw new Error("QQ 接收器未就绪就停止");
      }),
    ]);
    rpc = startPhoneRpc(
      new QqPhone(() => receiver.messages(), actions, timeline, {
        id: self.id,
        displayName: self.username ?? "",
      }),
      key,
      () => ({
        available: failure === undefined && !controller.signal.aborted && actions.available &&
          timeline.available,
        gateway: status,
        replyScope: "private",
      }),
    );
    let closing: Promise<void> | undefined;
    return {
      url: rpc.url,
      stopped: receive,
      close: () => {
        closing ??= (async () => {
          controller.abort();
          client.close();
          const problems: unknown[] = [];
          // 先停接收和 HTTP，再释放发送/索引锁；不与迟到处理器并写。
          for (
            const task of [
              () => rpc!.close(),
              () => receive!,
              () => actions.close(),
              () => timeline.close(),
            ]
          ) {
            try {
              await task();
            } catch (error) {
              problems.push(error);
            }
          }
          if (failure !== undefined || problems.length) {
            throw new Error("QQ 适配器停止时有未完成项，原件保留");
          }
        })();
        return closing;
      },
    };
  } catch (error) {
    controller.abort();
    client.close();
    await Promise.allSettled([rpc?.close(), receive]);
    await actions.close().catch(() => {});
    await timeline.close().catch(() => {});
    throw error;
  }
}
