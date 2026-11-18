import assert from "node:assert/strict";
import { createQqPlatformRead } from "../adapters/qq/platform-client.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { renderScriptContracts } from "../composition/script-docs.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import { PHONE_METHODS } from "../contracts/phone-rpc.ts";
import type {
  PhonePlatformCapabilities,
  PhonePlatformEvents,
} from "../contracts/phone-platform.ts";
import type { ScriptApi, ScriptSummary } from "../contracts/script.ts";
import { Kernel } from "../kernel/kernel.ts";
import type { Json } from "../kernel/protocol.ts";
import { SCRIPT_TOOL_METHODS } from "../services/admin/tool-catalog.ts";
import { settleScript } from "./helpers/scripts.ts";

const platformMethods = [
  "platformCapabilities",
  "platformEvents",
  "platformRead",
] as const satisfies readonly (keyof ScriptApi["phone"])[];

Deno.test("QQ 平台目录按需发现，新版不铺开路由；旧 pinned 文档按原字节返回", async () => {
  const requested: string[] = [];
  const pinned = "旧工具\r\n\r\n  保留末尾空白  \n";
  const rendered = await renderScriptContracts((path) => {
    requested.push(path);
    if (path === "contracts/script.ts") {
      return Promise.resolve("export const COMPACT_TOOL_PROMPT_VERSION = 1;\r\n");
    }
    assert.equal(path, "prompts/script-tools.md");
    return Promise.resolve(pinned);
  });
  assert.equal(rendered, pinned);
  assert.deepEqual(requested, ["contracts/script.ts", "prompts/script-tools.md"]);

  const current = await renderScriptContracts();
  for (const method of platformMethods) assert.ok(current.includes(method));
  assert.ok(current.includes('scripts.contracts({name:"phone-platform"})'));
  assert.doesNotMatch(current, /\/v2\/groups\/|\/guilds\/|interface PhonePlatform/);
});

Deno.test("QQ 平台三项只读工具通过真实脚本沙箱、宿主白名单及 phone 插件，operator 不注册", async () => {
  assert.deepEqual(
    PHONE_METHODS.filter((method) => method.startsWith("platform")),
    platformMethods,
  );
  assert.deepEqual(SCRIPT_TOOL_METHODS.phone, PHONE_METHODS);
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-platform-contract-" });
  const key = "local_platform_fixture_key_0123456789012345";
  const received: { method: string; args: Json }[] = [];
  const capabilities = {
    items: [{
      operation: "getMe",
      access: "read",
      chatReadable: true,
      method: "GET",
      surface: "v2",
    }],
  } satisfies PhonePlatformCapabilities;
  const events = {
    items: [{ seq: 8, receivedAt: "2026-01-01T00:00:00Z", type: "FRIEND_ADD" }],
    throughSeq: 8,
    hasMore: false,
  } satisfies PhonePlatformEvents;
  const calls: string[] = [];
  const read = createQqPlatformRead({
    catalog: {
      lookup: (operation) =>
        operation === "getMe"
          ? { access: "read", method: "GET", path: "/users/@me", queryParams: [], bodyParams: [] }
          : operation === "getGuilds"
          ? {
            access: "read",
            method: "GET",
            path: "/users/@me/guilds",
            queryParams: [],
            bodyParams: [],
          }
          : {
            access: "admin",
            method: "POST",
            path: "/operator-only",
            queryParams: [],
            bodyParams: [],
          },
      build: () => ({ path: "/users/@me" }),
    },
    request: (method, path) => {
      calls.push(`${method} ${path}`);
      return Promise.resolve({
        httpStatus: 200,
        data: { id: "bot", bot: true, token: "not-public" },
      });
    },
  });
  let legacy = false;
  const rpc = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    assert.equal(new URL(request.url).pathname, "/rpc");
    assert.equal(request.headers.get("authorization"), `Bearer ${key}`);
    const { method, args } = await request.json();
    if (method === "health") return Response.json({ ok: true, value: { available: true } });
    received.push({ method, args });
    if (method === "action") return Response.json({ ok: true, value: null });
    if (!legacy) {
      if (method === "platformCapabilities") {
        return Response.json({ ok: true, value: capabilities });
      }
      if (method === "platformEvents") return Response.json({ ok: true, value: events });
      if (method === "platformRead") return Response.json({ ok: true, value: await read(args) });
    }
    return Response.json({ ok: false, error: { category: "request" } });
  });
  const kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 30_000 });
  try {
    const config = `${directory}/config.json`;
    await Deno.writeTextFile(`${directory}/key`, key);
    await Deno.writeTextFile(
      config,
      JSON.stringify({
        phone: {
          adapterUrl: `http://127.0.0.1:${rpc.addr.port}/rpc`,
          adapterKeyFile: `${directory}/key`,
        },
        scripts: {
          directory: `${directory}/jobs`,
          cacheDirectory: `${directory}/cache`,
          denoBinary: Deno.execPath(),
          defaultTimeoutMs: 60_000,
        },
      }),
    );
    await composePhonePlugin(`${directory}/phone`, config);
    await composeScriptPlugin(`${directory}/scripts`, config);
    const manifest = await Deno.readTextFile(`${directory}/phone/manifest.json`);
    assert.equal(JSON.parse(manifest).provides.phone, "1.1.0");
    await kernel.register(`${directory}/phone`);
    await kernel.register(`${directory}/scripts`);
    await kernel.start();
    const code =
      `const directory = await tools.scripts.contracts({name:"phone-platform", limit:128});
      return { methods:Object.keys(tools.phone).filter(name => name.startsWith("platform")).sort(),
        directory:directory.nextChar, replies:[
          await tools.phone.platformCapabilities(), await tools.phone.platformEvents({ afterSeq: 7, limit: 2 }),
          await tools.phone.platformRead({ operation: "getMe", params: {} })
        ] };`;
    const started = await kernel.call<ScriptSummary & Json>("scripts", "start", {
      requestId: "local-platform-discovery",
      mode: "sync",
      code,
    });
    const finished = await settleScript(kernel, started);
    assert.equal(finished.status, "completed", JSON.stringify(finished.error));
    const page = await kernel.call<{ text: string }>("scripts", "result", { jobId: started.jobId });
    assert.deepEqual(JSON.parse(page.text), {
      directory: 128,
      methods: platformMethods,
      replies: [capabilities, events, {
        status: "ok",
        httpStatus: 200,
        data: { id: "bot", bot: true },
      }],
    });
    assert.deepEqual(received, [
      { method: "platformCapabilities", args: {} },
      { method: "platformEvents", args: { afterSeq: 7, limit: 2 } },
      { method: "platformRead", args: { operation: "getMe", params: {} } },
    ]);
    const denied = await settleScript(
      kernel,
      await kernel.call<ScriptSummary & Json>("scripts", "start", {
        requestId: "bad-platform-operation",
        mode: "sync",
        code: 'return await tools.phone.platformRead({ operation: "sendMessage", params: {} });',
      }),
    );
    assert.equal(denied.status, "failed");
    assert.equal(denied.scriptStarted, false);
    assert.equal(denied.error?.kind, "rejected");
    await assert.rejects(() => kernel.call("phone", "platformWrite", {}));
    assert.equal(received.length, 3);
    assert.deepEqual(
      await kernel.call("phone", "platformRead", { operation: "sendMessage", params: {} }),
      {
        status: "failed",
        reason: "not_allowed",
      },
    );
    assert.deepEqual(calls, ["GET /users/@me"]);
    legacy = true;
    await assert.rejects(
      () => kernel.call("phone", "platformCapabilities", {}),
      /platformCapabilities/,
    );
    assert.equal(await kernel.call("phone", "action", { actionId: "existing" }), null);
    assert.deepEqual(received.slice(-2).map((entry) => entry.method), [
      "platformCapabilities",
      "action",
    ]);
  } finally {
    await kernel.stop();
    await rpc.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
