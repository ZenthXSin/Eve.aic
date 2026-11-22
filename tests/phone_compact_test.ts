import assert from "node:assert/strict";
import { createPhonePlugin } from "../plugins/phone/mod.ts";
import type { Handler, Json, PluginContext } from "../sdk/plugin.ts";

Deno.test("phone brief: opt-in compaction keeps identity, unknown delivery, gaps, cursors and learned style", async () => {
  const directory = await Deno.makeTempDir();
  const calls: { method: string; args: Json }[] = [];
  const style = { forms: [], avoid: ["fake"] };
  const message = {
    handle: { conversationId: "qq:user:owner", seq: 2 },
    sender: { platformUserId: "impostor", displayName: { text: "owner" }, relationship: "owner" },
    direction: "incoming",
    time: { status: "missing" },
    body: { text: "page", truncated: true, characters: 100 },
    attachments: [{ index: 1, kind: "image", download: { state: "failed" } }],
    reference: { index: "unresolved" },
    delivery: { state: "unknown", actionId: "original-action" },
    chatStyle: style,
    mentions: [{ platformUserId: "owner", relationship: "other" }],
  };
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const call = await request.json();
    calls.push(call);
    return Response.json({
      ok: true,
      value: call.method === "health" ? { available: true } : {
        messages: [message],
        earlierMessagesNotShown: 1,
        readThroughSeq: 2,
        older: { beforeSeq: 2 },
        extension: { future: true },
      },
    });
  });
  let handlers: Record<string, Handler> = {};
  const cleanups: (() => void | Promise<void>)[] = [];
  const ctx = {
    provide(_contract: string, value: typeof handlers) {
      handlers = value;
    },
    onDisable(cleanup: () => void | Promise<void>) {
      cleanups.push(cleanup);
    },
  } as unknown as PluginContext;
  try {
    const keyPath = directory + "/key", configPath = directory + "/config.json";
    await Deno.writeTextFile(keyPath, "fixture_".repeat(4));
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        phone: {
          adapterUrl: "http://127.0.0.1:" + server.addr.port + "/rpc",
          adapterKeyFile: keyPath,
          ownerPlatformUserId: "owner",
        },
      }),
    );
    const plugin = createPhonePlugin(configPath);
    await plugin.load?.(ctx);
    await plugin.enable?.(ctx);
    const full = await handlers.open({ conversationId: "qq:user:owner" }) as Record<string, Json>;
    const fullMessage = (full.messages as Record<string, Json>[])[0];
    assert.equal((fullMessage.sender as Record<string, Json>).relationship, "other");
    assert.equal(fullMessage.body !== undefined, true);
    const brief = await handlers.open({ conversationId: "qq:user:owner", brief: true }) as Record<
      string,
      Json
    >;
    const compact = (brief.messages as Record<string, Json>[])[0];
    assert.equal(compact.rel, "other");
    assert.equal(compact.platformUserId, "impostor");
    assert.equal(compact.timeStatus, "missing");
    assert.equal(compact.truncated, true);
    assert.equal(compact.chars, 100);
    assert.deepEqual(compact.delivery, message.delivery);
    assert.deepEqual(compact.reference, message.reference);
    assert.deepEqual(compact.chatStyle, style);
    assert.equal((compact.mentions as Record<string, Json>[])[0].relationship, "owner");
    assert.equal(brief.earlierMessagesNotShown, 1);
    assert.equal(brief.readThroughSeq, 2);
    assert.deepEqual(brief.older, { beforeSeq: 2 });
    assert.deepEqual(brief.extension, { future: true });
    assert.equal(compact.body, undefined);
    assert.equal((calls.at(-1)!.args as Record<string, Json>).brief, undefined);
  } finally {
    for (const cleanup of cleanups.reverse()) await cleanup();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
