import assert from "node:assert/strict";
import { ChatModelError } from "../plugins/model-chat/client.ts";
import { permanentModelStatus, providerRejection } from "../sdk/model-failure.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import { RuntimeAdminClient, startRuntimeAdmin } from "../services/runtime/admin.ts";
import { KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "../sdk/durable-json.ts";

Deno.test("模型拒绝：只识别固定脱敏分类，暂时性HTTP仍可退避", () => {
  for (const status of [400, 401, 403, 404, 413, 422]) {
    assert.equal(permanentModelStatus(status), true);
    assert.equal(providerRejection(new ChatModelError("provider_rejected", status)), status);
    assert.equal(
      providerRejection(
        new KernelError("remote", `ChatModelError: 模型请求未完成：http (${status})`),
      ),
      status,
    );
  }
  for (const status of [200, 408, 409, 425, 429, 500, 503]) {
    assert.equal(permanentModelStatus(status), false);
    assert.equal(providerRejection(new ChatModelError("http", status)), undefined);
  }
  assert.equal(providerRejection(new Error("private: content-blocked")), undefined);
});

Deno.test("独立模型拒绝：在线scope跨重启持久、图片与观察隔离、管理恢复与普通resume分开", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-model-rejection-" });
  const settingsFile = `${root}/settings.json`;
  const settings = { dailyTokenBudget: 200_000_000, paused: false, timeZone: "Asia/Tokyo" };
  await writeDurableJson(settingsFile, settings);
  let attempts = 0;
  let reject = true;
  const options = {
    directory: `${root}/data`,
    workspace: `${root}/workspace`,
    settingsFile,
    maxOutputTokens: 100,
    model: {
      complete() {
        attempts++;
        // Replay the exact old release error envelope as a compatibility check.
        if (reject) throw new ChatModelError("http", 400);
        return Promise.resolve({ content: "fixture response", toolCalls: [] });
      },
      close() {},
    },
  };
  let engine = new RuntimeEngine(options);
  await engine.initialize();
  const request = { messages: [{ role: "user", content: "fixture" }], tools: [] };
  const key = "fixture_model_rejection_admin_0123456789";
  let drains = 0;
  const admin = () =>
    startRuntimeAdmin(engine, key, () => {
      drains++;
      return Promise.resolve();
    });
  let server = admin();
  await Deno.writeTextFile(`${root}/key`, key);
  await writeDurableJson(`${root}/connection`, { url: server.url });
  const client = new RuntimeAdminClient({
    connectionFile: `${root}/connection`,
    keyFile: `${root}/key`,
  });
  try {
    // Image/observer failures do not stop the online consciousness scope.
    await assert.rejects(() => engine.dispatch("model", "complete", request, "observer_1"));
    assert.equal(engine.modelStatus().blocked, null);
    const image = {
      dataUrl: "data:image/png;base64,iVBORw0KGgo=",
      provenance: {
        kind: "phone_attachment",
        conversationId: "fixture",
        sourceSeq: 1,
        attachmentId: "fixture-1",
        sha256: "a".repeat(64),
        mimeType: "image/png",
      },
    };
    const beforeImage = attempts;
    await assert.rejects(() =>
      engine.dispatch("model", "describeImage", { image }, "image_rejected")
    );
    assert.equal(attempts, beforeImage + 1);
    assert.equal(engine.modelStatus().blocked, null);
    await assert.rejects(() => engine.dispatch("model", "complete", request, "model_1"));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    const paid = attempts;
    await assert.rejects(() => engine.dispatch("model", "complete", request, "model_2"));
    assert.equal(attempts, paid);
    reject = false;
    await engine.dispatch("model", "complete", request, "observer_2");
    assert.equal(attempts, paid + 1);
    await assert.rejects(() => engine.dispatch("model", "describeImage", { image: {} }, "image_1"));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    await assert.rejects(() => client.resumeModel("model_1"), KernelError);
    assert.equal(drains, 0);
    await client.settings({ paused: true });
    await client.settings({ paused: false });
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    await server.close();
    await engine.close();
    engine = new RuntimeEngine(options);
    await engine.initialize();
    server = admin();
    await writeDurableJson(`${root}/connection`, { url: server.url });
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    await client.settings({ paused: true });
    await assert.rejects(() => client.resumeModel("model_999"), KernelError);
    assert.equal(drains, 0);
    await client.resumeModel("model_1");
    assert.equal(drains, 1);
    assert.equal(engine.modelStatus().blocked, null);
    assert.equal((await engine.budget.status()).paused, true);
    await server.close();
    await engine.close();
    engine = new RuntimeEngine(options);
    await engine.initialize();
    server = admin();
    assert.equal(engine.modelStatus().blocked, null);
    const marker = engine.records.events.at(-1)!;
    assert.equal(marker.kind, "model_resume");
    await assert.rejects(() =>
      engine.dispatch("timeline", "append", {
        expectedSeq: engine.records.events.length,
        expectedPrevious: marker.digest,
        kind: "model_resume",
        data: {},
      }, "forged_resume"), KernelError);
    await engine.budget.updateSettings(settings);
    await engine.dispatch("model", "complete", request, "model_3");
    assert.equal(attempts, paid + 2);
  } finally {
    await server.close();
    await engine.close();
    await Deno.remove(root, { recursive: true });
  }
});
