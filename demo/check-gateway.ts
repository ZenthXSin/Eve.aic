// 一次真实官方网关检查：只鉴权/接收，不发送 QQ 消息、不调用模型。
// 入站是实际数据，保留在指定私有目录，不能当作测试垃圾删除。
import { QqReceiver } from "../adapters/qq/receiver.ts";
import { readQqTestClient } from "../composition/qq.ts";

if (Deno.args.length !== 2) throw new Error("需要私有配置文件和私有接收目录");
const client = await readQqTestClient(Deno.args[0]);
const receiver = new QqReceiver(client, Deno.args[1]);
const controller = new AbortController();
const started = Date.now();
let ready = false;
const timer = setTimeout(() => controller.abort(), 20_000);
try {
  await receiver.run(controller.signal, (status) => {
    console.log(
      JSON.stringify({
        ...status,
        elapsedMs: Date.now() - started,
        qqChatSend: false,
        modelCalled: false,
      }),
    );
    if (status.status === "ready" || status.status === "resumed") {
      ready = true;
      controller.abort();
    }
  });
  console.log(
    JSON.stringify({
      gatewayReady: ready,
      receivedMessages: receiver.messages().length,
      qqChatSend: false,
      eveOnline: false,
    }),
  );
  if (!ready) Deno.exitCode = 1;
} finally {
  clearTimeout(timer);
  client.close();
}
