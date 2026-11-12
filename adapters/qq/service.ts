import { QQ_OFFICIAL_PROTOCOL } from "./official/protocol.ts";
import { type QqModelCommandOptions, QqModelCommands } from "./model-command.ts";
import { QqSendActions } from "./actions.ts";
import { QqClient } from "./client.ts";
import { QqPhone } from "./phone.ts";
import { QqReceiver, type QqReceiverStatus } from "./receiver.ts";
import { startPhoneRpc } from "./rpc.ts";
import { QqTimeline } from "./timeline.ts";
import { QqPhoneReading } from "./reading.ts";
import { QqPhoneDevice } from "./device.ts";
import { QqPhoneDownloads } from "./downloads.ts";
import { ensurePhoneActivationBoundary } from "./activation.ts";
import type { OwnershipFence } from "../../sdk/ownership-fence.ts";
import { fingerprint } from "../../kernel/calls.ts";
import { phoneReceipt } from "./phone.ts";
import type { Json } from "../../kernel/protocol.ts";

/** 一份适配器进程的生命周期，模型和手机插件不在此进程运行。 */
export async function startQqPhoneAdapter(
  client: QqClient,
  directory: string,
  key: string,
  onStatus?: (status: QqReceiverStatus) => void,
  fence?: OwnershipFence,
  options: { captureActivationBoundary?: boolean; modelCommands?: QqModelCommandOptions } = {},
) {
  const controller = new AbortController();
  const actions = new QqSendActions(client, `${directory}/outbox`, client.sendScope);
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/phone-reading`);
  const device = new QqPhoneDevice(`${directory}/phone-device`);
  const downloads = new QqPhoneDownloads(`${directory}/downloads`);
  const modelCommands = options.modelCommands === undefined ? undefined : new QqModelCommands(
    `${directory}/model-commands`,
    actions,
    options.modelCommands,
  );
  const receiver = new QqReceiver(
    client,
    directory,
    modelCommands
      ? {
        initialized: (records) => modelCommands.initialize(records),
        persisted: (record) => modelCommands.offer(record),
      }
      : {},
  );
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
    await reading.initialize();
    await device.initialize();
    await downloads.initialize();
    client.useDownloadedAttachmentReader((path) => downloads.readBytes(path));
    // 只有原发送账本的真实记录可补结算；无证据的 admitted 读取仍保持待核对。
    await fence?.reconcile(async (permit) => {
      const saved = actions.status(permit.actionId);
      if (!saved || saved.status === "running") return undefined;
      return {
        outcome: saved.status === "unknown" ? "unknown" : "completed",
        resultHash: await fingerprint(phoneReceipt(saved) as unknown as Json),
      };
    });
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
    const phone = new QqPhone(
      () => receiver.messages().map((record) => modelCommands?.project(record) ?? record),
      actions,
      timeline,
      {
        id: self.id,
        displayName: self.username ?? "",
      },
      reading,
      Date.now,
      device,
      downloads,
    );
    if (options.captureActivationBoundary) await ensurePhoneActivationBoundary(phone, directory);
    rpc = startPhoneRpc(
      phone,
      key,
      () => ({
        available: failure === undefined && !controller.signal.aborted && actions.available &&
          timeline.available && reading.available && device.available && downloads.available &&
          (!modelCommands || modelCommands.available),
        gateway: status,
        officialProtocol: QQ_OFFICIAL_PROTOCOL,
        replyScope: client.sendScope,
      }),
      fence,
    );
    let closing: Promise<void> | undefined;
    return {
      url: rpc.url,
      actions,
      stopped: receive,
      close: () => {
        closing ??= (async () => {
          modelCommands?.requestStop();
          actions.requestStop();
          controller.abort();
          client.close();
          const problems: unknown[] = [];
          // 先停接收和 HTTP，再释放发送/索引锁；不与迟到处理器并写。
          for (
            const task of [
              () => rpc!.close(),
              () => receive!,
              () => modelCommands?.close(),
              () => fence?.close(),
              () => actions.close(),
              () => timeline.close(),
              () => reading.close(),
              () => device.close(),
              () => downloads.close(),
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
    modelCommands?.requestStop();
    actions.requestStop();
    controller.abort();
    client.close();
    await Promise.allSettled([rpc?.close(), receive]);
    await modelCommands?.close().catch(() => {});
    await fence?.close().catch(() => {});
    await actions.close().catch(() => {});
    await timeline.close().catch(() => {});
    await reading.close().catch(() => {});
    await device.close().catch(() => {});
    await downloads.close().catch(() => {});
    throw error;
  }
}
