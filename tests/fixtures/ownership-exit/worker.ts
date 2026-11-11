// 实际子进程故障探针：执行前后都留事实，由父测试在指定边界 SIGKILL。
import { OwnershipExitClient, OwnershipRpcClient } from "../../../guardian/rpc.ts";
import { KernelError } from "../../../kernel/protocol.ts";
import { writeDurableJson } from "../../../sdk/durable-json.ts";
import { OwnershipFence } from "../../../sdk/ownership-fence.ts";

if (import.meta.main) {
  const config = JSON.parse(await Deno.readTextFile(Deno.args[0]));
  const client = new OwnershipRpcClient(config.url, config.exitKey, 1000);
  const fence = new OwnershipFence(config.fence, "phone", new OwnershipExitClient(client));
  await fence.initialize();
  try {
    await fence.run(config.proof, "platform-send-1", { content: "offline fixture" }, async () => {
      console.log(JSON.stringify({ status: "operation_started" }));
      while (true) {
        try {
          await Deno.stat(`${config.business}/release`);
          break;
        } catch (error) {
          if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
        await new Promise((done) => setTimeout(done, 10));
      }
      await writeDurableJson(`${config.business}/actual-receipt.json`, {
        sent: true,
        messageId: "offline-actual-1",
        executions: 1,
      });
      console.log(JSON.stringify({ status: "receipt_saved" }));
      if (config.waitAfterReceipt) await new Promise(() => {});
      return { sent: true, messageId: "offline-actual-1", executions: 1 };
    });
    console.log(JSON.stringify({ status: "operation_settled" }));
  } catch (error) {
    if (!(error instanceof KernelError) || error.kind !== "unknown") throw error;
    console.log(JSON.stringify({ status: "settlement_unknown" }));
  } finally {
    await fence.close();
    client.close();
  }
}
