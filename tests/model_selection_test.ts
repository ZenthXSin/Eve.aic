import assert from "node:assert/strict";
import { KernelError } from "../kernel/protocol.ts";
import type { ImageDescription, ModelImage } from "../contracts/model.ts";
import { ModelSelection } from "../services/runtime/model-selection.ts";
import { RuntimeEngine } from "../services/runtime/engine.ts";
import {
  RuntimeModelControlClient,
  startRuntimeModelControl,
} from "../services/runtime/model-control.ts";
import { FactJournal } from "../sdk/fact-journal.ts";

const id = (n: number) => `qq_model_${String(n).padStart(64, "0")}`;
const request = { messages: [{ role: "user", content: "hello" }], tools: [] };
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-model-selection-" });
  const bodies: { model: string }[] = [];
  let modelsAvailable = true;
  let rejectedModel: string | undefined;
  let gate: Promise<void> | undefined, release: (() => void) | undefined;
  let arrived: (() => void) | undefined;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    assert.equal(r.headers.get("authorization"), "Bearer fixture_secret");
    if (new URL(r.url).pathname === "/v1/models") {
      return modelsAvailable
        ? Response.json({
          data: [
            { id: "model-a" },
            { id: "model-b", supported_endpoint_types: ["openai"] },
            { id: "image-only", supported_endpoint_types: ["image-generation"] },
          ],
        })
        : new Response("no", { status: 503 });
    }
    const body = await r.json();
    const rejected = rejectedModel === body.model;
    bodies.push(body);
    arrived?.();
    if (gate) {
      const old = gate;
      gate = undefined;
      await old;
    }
    if (rejected) return new Response(null, { status: 401 });
    return Response.json({
      choices: [{
        finish_reason: "stop",
        message: { role: "assistant", content: `from ${body.model}` },
      }],
      usage: { prompt_tokens: 8, completion_tokens: 2 },
    });
  });
  const config = {
    model: "model-a",
    apiKey: "fixture_secret",
    baseUrl: `http://127.0.0.1:${server.addr.port}`,
    sessionId: "fixture_session",
    timeoutMs: 3000,
    maxOutputTokens: 100,
  };
  let selection = new ModelSelection(`${directory}/selection`, config);
  await selection.initialize();
  return {
    directory,
    config,
    bodies,
    selection: () => selection,
    rejectModel(model?: string) {
      rejectedModel = model;
    },
    unavailable() {
      modelsAvailable = false;
    },
    hold() {
      gate = new Promise<void>((r) => release = r);
      return new Promise<void>((r) => arrived = r);
    },
    release() {
      release?.();
    },
    async restart() {
      await selection.close();
      selection = new ModelSelection(`${directory}/selection`, config);
      await selection.initialize();
    },
    async close() {
      release?.();
      await selection.close();
      await server.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}
Deno.test("模型切换：仅列当前凭据可用Chat模型，真实修订与幂等回执跨重启保留", async () => {
  const f = await fixture();
  try {
    assert.deepEqual(await f.selection().status(), {
      current: { model: "model-a", revision: 0 },
      availableModels: ["model-a", "model-b"],
    });
    const result = await f.selection().select(id(1), "model-b");
    assert.deepEqual(result, { actionId: id(1), model: "model-b", revision: 1, changed: true });
    assert.deepEqual(await f.selection().select(id(1), "model-b"), result);
    await assert.rejects(
      f.selection().select(id(1), "model-a"),
      (e: unknown) => e instanceof KernelError && e.kind === "rejected",
    );
    const unchanged = await f.selection().select(id(2), "model-b");
    assert.equal(unchanged.revision, 1);
    assert.equal(unchanged.changed, false);
    await f.restart();
    assert.deepEqual(f.selection().receipt(id(1)), result);
    assert.deepEqual(f.selection().current(), { model: "model-b", revision: 1 });
  } finally {
    await f.close();
  }
});
Deno.test("模型切换：未知模型与不可读列表均拒绝新切换，不改变已提交选择", async () => {
  const f = await fixture();
  try {
    await assert.rejects(
      f.selection().select(id(1), "invented"),
      (e: unknown) => e instanceof KernelError && e.kind === "rejected",
    );
    f.unavailable();
    await assert.rejects(
      f.selection().select(id(2), "model-b"),
      (e: unknown) => e instanceof KernelError && e.kind === "unavailable",
    );
    assert.deepEqual(f.selection().current(), { model: "model-a", revision: 0 });
    assert.equal(f.selection().journal.events.length, 1);
  } finally {
    await f.close();
  }
});
Deno.test("模型切换：已受理未确认动作重启后保持unknown，不能以新目标或同ID重放", async () => {
  const f = await fixture();
  try {
    await f.selection().close();
    const j = new FactJournal(`${f.directory}/selection`);
    await j.initialize();
    await j.append("request", { actionId: id(1), model: "model-b" });
    await j.close();
    await f.restart();
    await assert.rejects(
      f.selection().select(id(1), "model-b"),
      (e: unknown) => e instanceof KernelError && e.kind === "unknown",
    );
    assert.equal(f.bodies.length, 0);
    assert.deepEqual(f.selection().current(), { model: "model-a", revision: 0 });
  } finally {
    await f.close();
  }
});
Deno.test("模型控制：独立凭据、绑定主人、Origin拒绝及原动作receipt", async () => {
  const f = await fixture(), key = "k".repeat(64);
  const server = startRuntimeModelControl(f.selection(), key, "owner-openid");
  try {
    await Deno.writeTextFile(`${f.directory}/connection.json`, JSON.stringify({ url: server.url }));
    await Deno.writeTextFile(`${f.directory}/key`, key);
    const client = new RuntimeModelControlClient({
      connectionFile: `${f.directory}/connection.json`,
      keyFile: `${f.directory}/key`,
      ownerPlatformUserId: "owner-openid",
    });
    assert.equal((await client.status()).current.model, "model-a");
    const changed = await client.select(id(1), "model-b");
    assert.deepEqual(await client.receipt(id(1)), changed);
    const other = new RuntimeModelControlClient({ ...client.config, ownerPlatformUserId: "other" });
    await assert.rejects(
      other.select(id(2), "model-a"),
      (e: unknown) => e instanceof KernelError && e.kind === "rejected",
    );
    for (
      const headers of [{ authorization: `Bearer ${key}`, origin: "https://bad.example" }, {
        authorization: "Bearer wrong",
      }]
    ) {
      const response = await fetch(server.url, {
        method: "POST",
        headers: headers as Record<string, string>,
        body: JSON.stringify({ method: "status", args: { ownerPlatformUserId: "owner-openid" } }),
      });
      assert.equal(response.status, 403);
      await response.body?.cancel();
    }
  } finally {
    await server.close();
    await f.close();
  }
});
Deno.test("模型切换：在途图像保留旧模型来源，新主请求和下一图像使用新模型；旧回执不重放", async () => {
  const f = await fixture();
  await Deno.writeTextFile(
    `${f.directory}/settings.json`,
    JSON.stringify({ dailyTokenBudget: 1000000, paused: false, timeZone: "Asia/Tokyo" }),
  );
  const snapshot = f.selection().snapshot();
  const engine = new RuntimeEngine({
    directory: `${f.directory}/runtime`,
    workspace: `${f.directory}/workspace`,
    settingsFile: `${f.directory}/settings.json`,
    model: snapshot.client,
    modelName: snapshot.modelName,
    maxOutputTokens: 100,
    modelSelection: f.selection(),
  });
  await engine.initialize();
  const image: ModelImage = {
    dataUrl:
      "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5ioAAAAASUVORK5CYII=",
    provenance: {
      kind: "phone_attachment",
      conversationId: "qq:user:owner",
      sourceSeq: 1,
      attachmentId: "image-1",
      mimeType: "image/png",
      sha256: "",
    },
  };
  const raw = Uint8Array.from(atob(image.dataUrl.split(",")[1]), (c) => c.charCodeAt(0));
  image.provenance.sha256 = Array.from(
    new Uint8Array(await crypto.subtle.digest("SHA-256", raw)),
    (n) => n.toString(16).padStart(2, "0"),
  ).join("");
  try {
    const arrived = f.hold();
    const first = engine.dispatch("model", "describeImage", { image } as never, "image_1");
    await arrived;
    await f.selection().select(id(1), "model-b");
    f.release();
    const result = await first as unknown as ImageDescription;
    assert.equal(result.model, "model-a");
    assert.deepEqual(
      await engine.dispatch("model", "describeImage", { image } as never, "image_1"),
      result,
    );
    await engine.dispatch("model", "complete", request as never, "main_after_switch");
    const next = await engine.dispatch(
      "model",
      "describeImage",
      { image } as never,
      "image_2",
    ) as unknown as ImageDescription;
    assert.equal(next.model, "model-b");
    assert.deepEqual(f.bodies.map((b) => b.model), ["model-a", "model-b", "model-b"]);
    assert.equal(
      (engine.transactions.events[0].data as { recovery: { modelName: string } }).recovery
        .modelName,
      "model-a",
    );
    assert.equal(
      (engine.modelResponses.events[0].data as { modelName: string }).modelName,
      "model-a",
    );
  } finally {
    await engine.close();
    await f.close();
  }
});

async function selectedEngine(f: Awaited<ReturnType<typeof fixture>>) {
  await Deno.writeTextFile(
    `${f.directory}/settings.json`,
    JSON.stringify({ dailyTokenBudget: 1000000, paused: false, timeZone: "Asia/Tokyo" }),
  );
  const snapshot = f.selection().snapshot();
  const engine = new RuntimeEngine({
    directory: `${f.directory}/runtime`,
    workspace: `${f.directory}/workspace`,
    settingsFile: `${f.directory}/settings.json`,
    model: { complete: (r) => snapshot.client.complete(r), close() {} },
    modelName: f.config.model,
    maxOutputTokens: 100,
    modelSelection: f.selection(),
  });
  await engine.initialize();
  return engine;
}
Deno.test("模型切换：显式选择不同模型解除旧拒绝且持久记录，同模型不解除；旧失败回执保持原件", async () => {
  const f = await fixture();
  let engine = await selectedEngine(f);
  try {
    f.rejectModel("model-a");
    await assert.rejects(engine.dispatch("model", "complete", request as never, "model_1"));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    await engine.modelSelectionChanged(await f.selection().select(id(1), "model-a"));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    await engine.modelSelectionChanged(await f.selection().select(id(2), "model-b"));
    assert.equal(engine.modelStatus().blocked, null);
    const resume = engine.records.events.find((e) => e.kind === "model_resume");
    assert.equal((resume?.data as { actionId: string }).actionId, id(2));
    await assert.rejects(engine.dispatch("model", "complete", request as never, "model_1"));
    await engine.dispatch("model", "complete", request as never, "model_2");
    assert.deepEqual(f.bodies.map((b) => b.model), ["model-a", "model-b"]);
    await engine.close();
    await f.restart();
    engine = await selectedEngine(f);
    assert.equal(engine.modelStatus().blocked, null);
    await engine.modelSelectionChanged(await f.selection().select(id(3), "model-a"));
    assert.equal(engine.modelStatus().blocked, null);
  } finally {
    await engine.close();
    await f.close();
  }
});
Deno.test("模型切换：旧模型在途拒绝晚到不阻塞新选择，新选择自身拒绝跨重启保留", async () => {
  const f = await fixture();
  let engine = await selectedEngine(f);
  try {
    f.rejectModel("model-a");
    const arrived = f.hold();
    const first = engine.dispatch("model", "complete", request as never, "model_1");
    await arrived;
    await engine.modelSelectionChanged(await f.selection().select(id(1), "model-b"));
    f.release();
    await assert.rejects(first);
    assert.equal(engine.modelStatus().blocked, null);
    await engine.dispatch("model", "complete", request as never, "model_2");
    f.rejectModel("model-b");
    await assert.rejects(engine.dispatch("model", "complete", request as never, "model_3"));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_3");
    await engine.close();
    await f.restart();
    engine = await selectedEngine(f);
    assert.equal(engine.modelStatus().blocked?.requestId, "model_3");
    await engine.modelSelectionChanged(await f.selection().select(id(2), "model-a"));
    assert.equal(engine.modelStatus().blocked, null);
  } finally {
    await engine.close();
    await f.close();
  }
});

Deno.test("模型控制：持有正确密钥也拒绝Host重绑定、跨站请求和非精确入口", async () => {
  const f = await fixture(), key = "h".repeat(64);
  const server = startRuntimeModelControl(f.selection(), key, "owner-openid");
  try {
    const body = JSON.stringify({
      method: "status",
      args: { ownerPlatformUserId: "owner-openid" },
    });
    const deniedHeaders: Record<string, string>[] = [
      { host: "rebind.example" },
      { host: "localhost:" + new URL(server.url).port },
      { host: "127.0.0.1:1" },
      { "sec-fetch-site": "cross-site" },
      { "sec-fetch-site": "same-site" },
    ];
    // fetch normalizes forbidden Host/Sec-Fetch headers; use real HTTP bytes for rebinding.
    const rawStatus = async (extra: Record<string, string>, target = "/runtime-model-control") => {
      const url = new URL(server.url);
      const connection = await Deno.connect({ hostname: "127.0.0.1", port: Number(url.port) });
      try {
        const headers = { host: url.host, authorization: `Bearer ${key}`, ...extra };
        const wire = `POST ${target} HTTP/1.1\r\n${
          Object.entries(headers).map(([k, v]) => `${k}: ${v}`).join("\r\n")
        }\r\nContent-Length: ${
          new TextEncoder().encode(body).length
        }\r\nConnection: close\r\n\r\n${body}`;
        const bytes = new TextEncoder().encode(wire);
        let offset = 0;
        while (offset < bytes.length) offset += await connection.write(bytes.subarray(offset));
        let response = "";
        const buffer = new Uint8Array(4096);
        while (true) {
          const count = await connection.read(buffer);
          if (count === null) break;
          response += new TextDecoder().decode(buffer.subarray(0, count));
        }
        return Number(/^HTTP\/1\.1 (\d{3})/.exec(response)?.[1]);
      } finally {
        connection.close();
      }
    };
    for (const extra of deniedHeaders) {
      assert.equal(await rawStatus(extra), 403, JSON.stringify(extra));
    }
    assert.equal(await rawStatus({}, "http://rebind.example/runtime-model-control"), 403);
    const response = await fetch(server.url + "?unexpected=1", {
      method: "POST",
      body,
      headers: { authorization: `Bearer ${key}` },
    });
    assert.equal(response.status, 403);
    await response.body?.cancel();
    assert.equal(f.selection().journal.events.length, 1);
  } finally {
    await server.close();
    await f.close();
  }
});

Deno.test("模型控制：选择落盘后apply失败可用原receipt补齐；并发receipt只apply一次，旧修订不清新block", async () => {
  const f = await fixture(), key = "a".repeat(64);
  let engine = await selectedEngine(f);
  let failOnce = true;
  const server = startRuntimeModelControl(f.selection(), key, "owner-openid", async (result) => {
    if (failOnce) {
      failOnce = false;
      throw new KernelError("unknown", "fixture interrupted after selection commit");
    }
    await engine.modelSelectionChanged(result);
  });
  try {
    await Deno.writeTextFile(`${f.directory}/connection.json`, JSON.stringify({ url: server.url }));
    await Deno.writeTextFile(`${f.directory}/key`, key);
    const client = new RuntimeModelControlClient({
      connectionFile: `${f.directory}/connection.json`,
      keyFile: `${f.directory}/key`,
      ownerPlatformUserId: "owner-openid",
    });
    f.rejectModel("model-a");
    await assert.rejects(engine.dispatch("model", "complete", request as never, "model_1"));
    await assert.rejects(
      client.select(id(1), "model-b"),
      (e: unknown) => e instanceof KernelError && e.kind === "unknown",
    );
    assert.deepEqual(f.selection().current(), { model: "model-b", revision: 1 });
    assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
    const receipts = await Promise.all([client.receipt(id(1)), client.receipt(id(1))]);
    assert.deepEqual(receipts[0], receipts[1]);
    assert.equal(engine.modelStatus().blocked, null);
    assert.equal(engine.records.events.filter((e) => e.kind === "model_resume").length, 1);
    f.rejectModel("model-b");
    await assert.rejects(engine.dispatch("model", "complete", request as never, "model_2"));
    await client.receipt(id(1));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_2");
    await client.select(id(2), "model-a");
    f.rejectModel("model-a");
    await assert.rejects(engine.dispatch("model", "complete", request as never, "model_3"));
    await client.receipt(id(1));
    assert.equal(engine.modelStatus().blocked?.requestId, "model_3");
    assert.equal(engine.records.events.filter((e) => e.kind === "model_resume").length, 2);
    assert.equal(f.selection().journal.events.length, 5);
    await server.close();
    await engine.close();
    await f.restart();
    engine = await selectedEngine(f);
    assert.equal(engine.modelStatus().blocked?.requestId, "model_3");
    assert.deepEqual(f.selection().current(), { model: "model-a", revision: 2 });
  } finally {
    await server.close();
    await engine.close();
    await f.close();
  }
});

for (const newModelRejects of [false, true]) {
  Deno.test(`模型切换准入竞态：旧block恢复落盘等待时新模型${newModelRejects ? "真实拒绝仍持久阻塞" : "正常调用且不伪造永久拒绝"}`, async () => {
    const f = await fixture();
    let engine = await selectedEngine(f);
    let release!: () => void;
    const barrier = new Promise<void>((r) => release = r);
    let entered!: () => void;
    const waiting = new Promise<void>((r) => entered = r);
    const append = engine.records.append.bind(engine.records);
    engine.records.append = async (kind, data) => {
      if (
        kind === "model_resume" &&
        (data as { reason?: string }).reason === "owner_explicit_model_switch"
      ) {
        entered();
        await barrier;
      }
      return await append(kind, data);
    };
    let apply: Promise<void> | undefined;
    try {
      f.rejectModel("model-a");
      await assert.rejects(engine.dispatch("model", "complete", request as never, "model_1"));
      assert.equal(engine.modelStatus().blocked?.requestId, "model_1");
      const selected = await f.selection().select(id(1), "model-b");
      apply = engine.modelSelectionChanged(selected);
      await waiting;
      if (newModelRejects) f.rejectModel("model-b");
      const next = engine.dispatch("model", "complete", request as never, "model_2");
      if (newModelRejects) {
        await assert.rejects(next);
        assert.equal(engine.modelStatus().blocked?.requestId, "model_2");
      } else {
        assert.equal((await next as { content: string }).content, "from model-b");
        assert.equal(engine.receipt("model_2")?.error, undefined);
      }
      // The second provider call must actually be B, never a locally copied A rejection.
      assert.deepEqual(f.bodies.map((body) => body.model), ["model-a", "model-b"]);
      release();
      await apply;
      assert.equal(
        engine.modelStatus().blocked?.requestId ?? null,
        newModelRejects ? "model_2" : null,
      );
      await engine.close();
      await f.restart();
      engine = await selectedEngine(f);
      assert.equal(
        engine.modelStatus().blocked?.requestId ?? null,
        newModelRejects ? "model_2" : null,
      );
      assert.deepEqual(f.selection().current(), { model: "model-b", revision: 1 });
      const original = engine.receipt("model_1");
      assert.match(original?.error?.message ?? "", /provider_rejected \(401\)/);
    } finally {
      release();
      await apply;
      await engine.close();
      await f.close();
    }
  });
}
