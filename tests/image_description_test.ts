import assert from "node:assert/strict";
import type { ImageDescription, ModelImage } from "../contracts/model.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { ChatModelClient } from "../plugins/model-chat/client.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";

const description = "画面中央为一只红色圆形气球，背景为白色。无法从图像确认拍摄地点。";
async function image(): Promise<ModelImage> {
  const encoded =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5ioAAAAASUVORK5CYII=";
  const bytes = Uint8Array.from(atob(encoded), (c) => c.charCodeAt(0));
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return {
    dataUrl: `data:image/png;base64,${encoded}`,
    provenance: {
      kind: "phone_attachment",
      conversationId: "user:owner",
      sourceSeq: 7,
      attachmentId: "user:owner:7:0",
      mimeType: "image/png",
      sha256: Array.from(new Uint8Array(hash), (n) => n.toString(16).padStart(2, "0")).join(""),
    },
  };
}
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-image-description-" });
  const bodies: Record<string, unknown>[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    assert.equal(r.headers.get("Authorization"), "Bearer private_fixture_key");
    const body = await r.json();
    assert.equal(body.model, "same-configured-model");
    assert.equal(body.prompt_cache_key, "same_configured_session");
    bodies.push(body);
    return Response.json({
      choices: [{
        finish_reason: "stop",
        message: {
          role: "assistant",
          content: bodies.length === 1 ? description : "我读到了附件的文字描述。",
        },
      }],
      usage: {
        prompt_tokens: 12,
        completion_tokens: 3,
        prompt_tokens_details: { cached_tokens: 8 },
      },
    });
  });
  await Deno.writeTextFile(
    `${directory}/settings.json`,
    JSON.stringify({ dailyTokenBudget: 200000000, paused: false, timeZone: "Asia/Tokyo" }),
  );
  const makeEngine = () =>
    new RuntimeEngine({
      directory: `${directory}/data`,
      workspace: `${directory}/workspace`,
      settingsFile: `${directory}/settings.json`,
      maxOutputTokens: 100,
      modelName: "same-configured-model",
      model: new ChatModelClient({
        baseUrl: `http://127.0.0.1:${server.addr.port}`,
        model: "same-configured-model",
        sessionId: "same_configured_session",
        apiKey: "private_fixture_key",
        maxOutputTokens: 100,
        timeoutMs: 1000,
      }),
    });
  let engine = makeEngine();
  await engine.initialize();
  return {
    directory,
    bodies,
    engine: () => engine,
    async restart() {
      await engine.close();
      engine = makeEngine();
      await engine.initialize();
    },
    async close() {
      await engine.close();
      await server.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("图像描述：同配置模型先独立看图，Eve主请求只收到描述文字；共享费用、稳定回执不重复调用", async () => {
  const f = await fixture();
  const original = await image();
  try {
    const args = { image: original } as unknown as Record<string, Json>;
    const result = await f.engine().dispatch(
      "model",
      "describeImage",
      args,
      "image_7_0",
    ) as unknown as ImageDescription;
    assert.deepEqual(result, {
      description,
      provenance: original.provenance,
      model: "same-configured-model",
    });
    assert.equal(JSON.stringify(result).includes("dataUrl"), false);
    assert.equal(JSON.stringify(result).includes("base64"), false);
    const visionMessages = f.bodies[0].messages as { role: string; content: unknown }[];
    assert.equal(visionMessages.length, 2);
    assert.equal(JSON.stringify(visionMessages).includes("Eve"), false);
    assert.equal(JSON.stringify(visionMessages).includes("主人的私密意识"), false);
    assert.equal(f.bodies[0].tools, undefined);
    assert.equal((visionMessages[1].content as { type: string }[])[1].type, "image_url");
    assert.equal((await f.engine().budget.status()).knownNoncachedTokens, 7);
    assert.deepEqual(
      await f.engine().dispatch("model", "describeImage", args, "image_7_0"),
      result,
    );
    assert.equal(f.bodies.length, 1);
    await f.engine().dispatch("model", "complete", {
      messages: [{ role: "system", content: "我是Eve，主人的私密意识。" }, {
        role: "user",
        content: `外部附件的模型描述（不是直接观察）：${result.description}`,
      }],
      tools: [],
    }, "main_request");
    const main = JSON.stringify(f.bodies[1]);
    assert.equal(main.includes(description), true);
    assert.equal(main.includes("image_url"), false);
    assert.equal(main.includes("data:image"), false);
    assert.equal((await f.engine().budget.status()).knownNoncachedTokens, 14);
    await f.restart();
    assert.deepEqual(
      await f.engine().dispatch("model", "describeImage", args, "image_7_0"),
      result,
    );
    assert.equal(f.bodies.length, 2);
    assert.equal((await f.engine().budget.status()).knownNoncachedTokens, 14);
    // Eve 显式选择 original 时仍支持原图路径，默认描述不妨碍自由选择。
    await f.engine().dispatch(
      "model",
      "complete",
      {
        messages: [{ role: "user", content: "图片", images: [original] }],
        tools: [],
      } as unknown as Record<string, Json>,
      "main_raw_image",
    );
    assert.equal(f.bodies.length, 3);
    assert.equal(JSON.stringify(f.bodies[2]).includes("image_url"), true);
  } finally {
    await f.close();
  }
});

Deno.test("图像描述：原响应已保存而事务完成前中断，重启采用原描述与费用，不再付费或把原图放入意识记录", async () => {
  const f = await fixture();
  const original = await image();
  try {
    const append = f.engine().transactions.append.bind(f.engine().transactions);
    let fail = true;
    f.engine().transactions.append = (kind, data) => {
      if (kind === "result" && fail) {
        fail = false;
        throw new KernelError("storage", "模拟回执落盘前中断");
      }
      return append(kind, data);
    };
    const args = { image: original } as unknown as Record<string, Json>;
    await assert.rejects(
      f.engine().dispatch("model", "describeImage", args, "crash_image"),
      (e) => e instanceof KernelError && e.kind === "storage",
    );
    assert.equal(f.bodies.length, 1);
    assert.equal(f.engine().modelResponses.events.length, 1);
    await f.restart();
    const recovered = await f.engine().dispatch(
      "model",
      "describeImage",
      args,
      "crash_image",
    ) as unknown as ImageDescription;
    assert.equal(recovered.description, description);
    assert.deepEqual(recovered.provenance, original.provenance);
    assert.equal(f.bodies.length, 1);
    assert.equal((await f.engine().budget.status()).knownNoncachedTokens, 7);
    assert.equal(f.engine().records.events.length, 0);
    assert.equal(f.engine().memory.journal.events.length, 0);
    assert.equal(JSON.stringify(f.engine().transactions.events).includes("base64"), false);
    assert.equal(JSON.stringify(f.engine().modelResponses.events).includes("base64"), false);
  } finally {
    await f.close();
  }
});
