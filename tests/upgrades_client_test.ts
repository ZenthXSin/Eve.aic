import assert from "node:assert/strict";
import { settleScript } from "./helpers/scripts.ts";
import { UpgradeClient } from "../plugins/runtime-client/upgrades.ts";
import { composeRuntimePlugin } from "../composition/runtime.ts";
import { composeScriptPlugin } from "../composition/scripts.ts";
import { Kernel } from "../kernel/kernel.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import type { ScriptSummary } from "../contracts/script.ts";

const commit = "a".repeat(40);
const proof = { instanceId: "old", epoch: 2, credential: "private_instance_credential" };

Deno.test("升级客户端：真实RPC只传实例证明、每次重读连接与代际，坏参数/失联不重试", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-upgrades-client-" });
  const bodies: Record<string, unknown>[] = [];
  let broken = false;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    assert.equal(new URL(r.url).pathname, "/upgrade");
    const data = await r.json();
    assert.equal(r.headers.get("authorization"), `Bearer ${data.ownership.proof.credential}`);
    bodies.push(data);
    if (broken) return new Response("private upstream body", { status: 503 });
    return Response.json({
      ok: true,
      value: {
        actionId: data.args.actionId ?? data.ownership.actionId,
        commit,
        state: "submitted",
      },
    });
  });
  const client = new UpgradeClient({
    connectionFile: `${directory}/connection.json`,
    proofFile: `${directory}/proof.json`,
  });
  try {
    await Deno.writeTextFile(
      `${directory}/connection.json`,
      JSON.stringify({ url: `http://127.0.0.1:${server.addr.port}/upgrade` }),
    );
    await Deno.writeTextFile(`${directory}/proof.json`, JSON.stringify(proof));
    assert.deepEqual(await client.call("request", { commit }, "script_request_1"), {
      actionId: "script_request_1",
      commit,
      state: "submitted",
    });
    await Deno.writeTextFile(
      `${directory}/proof.json`,
      JSON.stringify({ ...proof, epoch: 3, credential: "new_private_credential" }),
    );
    await client.call("status", { actionId: "script_request_1" }, "script_read_2");
    assert.deepEqual(bodies[1], {
      method: "status",
      args: { actionId: "script_request_1" },
      ownership: {
        proof: { ...proof, epoch: 3, credential: "new_private_credential" },
        actionId: "script_read_2",
      },
    });
    for (const args of [{ commit: "main" }, { commit, guardianKey: "forbidden" }] as Json[]) {
      await assert.rejects(
        client.call("request", args, "bad"),
        (e) => e instanceof KernelError && e.kind === "rejected",
      );
    }
    await assert.rejects(
      client.call("status", { actionId: "" }, "bad"),
      (e) => e instanceof KernelError && e.kind === "rejected",
    );
    assert.equal(bodies.length, 2);
    broken = true;
    await assert.rejects(
      client.call("request", { commit }, "lost"),
      (e) =>
        e instanceof KernelError && e.kind === "unknown" &&
        !e.message.includes("private upstream body"),
    );
    assert.equal(bodies.length, 3);
    await Deno.writeTextFile(
      `${directory}/connection.json`,
      JSON.stringify({ url: "https://example.invalid/upgrade" }),
    );
    await assert.rejects(
      client.call("request", { commit }, "bad-url"),
      (e) => e instanceof KernelError && e.kind === "rejected",
    );
    assert.equal(bodies.length, 3);
    await Deno.writeTextFile(`${directory}/proof.json`, '{"credential":"private_secret" broken');
    await assert.rejects(
      client.call("request", { commit }, "bad-proof"),
      (e) =>
        e instanceof KernelError && e.kind === "unavailable" &&
        !e.message.includes("private_secret"),
    );
  } finally {
    client.close();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("升级脚本：真实worker经独立监督RPC，稳定动作编号且重试任务不重放，sendFile编号也由宿主提供", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-upgrades-script-" });
  const calls: Record<string, unknown>[] = [];
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (r) => {
    const data = await r.json();
    if (new URL(r.url).pathname === "/runtime") {
      assert.equal(data.namespace, "health");
      return Response.json({ ok: true, value: { available: true, ownershipFenced: true } });
    }
    assert.equal(new URL(r.url).pathname, "/upgrade");
    assert.equal(r.headers.get("authorization"), `Bearer ${proof.credential}`);
    assert.deepEqual(data.ownership.proof, proof);
    calls.push(data);
    return Response.json({
      ok: true,
      value: {
        actionId: data.args.actionId ?? data.ownership.actionId,
        state: "submitted",
        commit,
      },
    });
  });
  let kernel: Kernel | undefined;
  try {
    for (
      const [file, value] of Object.entries({
        "runtime.json": { url: `http://127.0.0.1:${server.addr.port}/runtime` },
        "upgrade.json": { url: `http://127.0.0.1:${server.addr.port}/upgrade` },
        "proof.json": proof,
      })
    ) await Deno.writeTextFile(`${directory}/${file}`, JSON.stringify(value));
    await Deno.writeTextFile(`${directory}/key`, "private_runtime_key");
    const configPath = `${directory}/config.json`;
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        runtime: {
          connectionFile: `${directory}/runtime.json`,
          keyFile: `${directory}/key`,
          proofFile: `${directory}/proof.json`,
          upgradeConnectionFile: `${directory}/upgrade.json`,
        },
        scripts: {
          directory: `${directory}/scripts`,
          cacheDirectory: `${directory}/cache`,
          denoBinary: Deno.execPath(),
          defaultTimeoutMs: 60_000,
        },
      }),
    );
    await composeRuntimePlugin(`${directory}/runtime-plugin`, configPath);
    await composeScriptPlugin(`${directory}/script-plugin`, configPath);
    await Deno.mkdir(`${directory}/phone`);
    await Deno.writeTextFile(
      `${directory}/phone/manifest.json`,
      JSON.stringify({ name: "fixture-phone", version: "1.0.0", provides: { phone: "1.0.0" } }),
    );
    await Deno.writeTextFile(
      `${directory}/phone/mod.ts`,
      `export default {load(ctx){ctx.provide("phone",{sendFile:args=>({actionId:args.actionId,state:"sent"})})}};`,
    );
    kernel = new Kernel({ dataDir: `${directory}/kernel`, callTimeoutMs: 20000 });
    for (const dir of ["phone", "runtime-plugin", "script-plugin"]) {
      await kernel.register(`${directory}/${dir}`);
    }
    await kernel.start();
    const request = {
      requestId: "upgrade_test",
      mode: "sync",
      code:
        `const submitted=await tools.upgrades.request({commit:"${commit}"}); const status=await tools.upgrades.status();const logs=await tools.upgrades.logs();const sent=await tools.phone.sendFile({conversationId:"user:owner",path:"file.txt",kind:"file"});return {submitted,status,logs,sent};`,
    };
    const job = await settleScript(
      kernel,
      await kernel.call("scripts", "start", request) as unknown as ScriptSummary,
    );
    assert.equal(job.status, "completed");
    assert.equal(job.actions, 4);
    assert.equal(calls.length, 3);
    assert.equal((calls[0].ownership as { actionId: string }).actionId, `script_${job.jobId}_1`);
    assert.deepEqual(calls[0].args, { commit });
    assert.deepEqual(calls[1].args, {});
    assert.equal(calls[2].method, "logs");
    assert.deepEqual(calls[2].args, {});
    const result = await kernel.call("scripts", "result", { jobId: job.jobId }) as { text: string };
    assert.equal(JSON.parse(result.text).sent.actionId, `script_${job.jobId}_4`);
    assert.equal(JSON.stringify(result).includes(proof.credential), false);
    assert.deepEqual(await kernel.call("scripts", "start", request), job);
    assert.equal(calls.length, 3);
    const forged = await settleScript(
      kernel,
      await kernel.call("scripts", "start", {
        requestId: "forged_upgrade",
        mode: "sync",
        code:
          `const args={commit:"${commit}",_actionId:"forged"};return await tools.upgrades.request(args);`,
      }) as unknown as ScriptSummary,
    );
    assert.equal(forged.status, "failed");
    assert.equal(calls.length, 3);
    const forgedFile = await settleScript(
      kernel,
      await kernel.call("scripts", "start", {
        requestId: "forged_file",
        mode: "sync",
        code:
          'const args={conversationId:"user:owner",path:"file.txt",kind:"file" as const,actionId:"forged"};return await tools.phone.sendFile(args);',
      }) as unknown as ScriptSummary,
    );
    assert.equal(forgedFile.status, "failed");
  } finally {
    if (kernel) await kernel.stop();
    await server.shutdown();
    await Deno.remove(directory, { recursive: true });
  }
});
