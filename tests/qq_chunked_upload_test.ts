import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { QqClient } from "../adapters/qq/client.ts";

const bytes = Uint8Array.from({ length: 5 * 1024 * 1024 + 37 }, (_, i) => i % 251);
const hash = (b: Uint8Array, alg = "md5") => createHash(alg).update(b).digest("hex");
for (
  const mode of [
    "success",
    "put",
    "finish",
    "merge",
    "duplicate",
    "short",
    "external",
    "timeout",
    "close",
    "unknown",
  ] as const
) {
  Deno.test(`QQ chunked upload: ${mode}`, async () => {
    let messages = 0, puts = 0, finishes = 0, merges = 0;
    const size = 3 * 1024 * 1024;
    const server = Deno.serve(
      { hostname: "127.0.0.1", port: 0, onListen() {} },
      async (r): Promise<Response> => {
        const path = new URL(r.url).pathname;
        if (path === "/app/getAppAccessToken") {
          return Response.json({ access_token: "secret-token", expires_in: 7200 });
        }
        if (path.startsWith("/part/")) {
          puts++;
          assert.equal(r.headers.has("authorization"), false);
          assert.equal(r.method, "PUT");
          const index = Number(path.split("/").pop());
          assert.deepEqual(
            new Uint8Array(await r.arrayBuffer()),
            bytes.subarray((index - 1) * size, index * size),
          );
          if (mode === "close") client.close();
          if (mode === "timeout" || mode === "close") {
            await new Promise((resolve) => setTimeout(resolve, 100));
          }
          return new Response(null, { status: mode === "put" ? 500 : 200 });
        }
        assert.equal(r.headers.get("authorization"), "QQBot secret-token");
        const body = await r.json();
        if (path.endsWith("upload_prepare")) {
          assert.deepEqual(body, {
            file_type: 4,
            file_size: String(bytes.length),
            file_name: "test.zip",
            md5: hash(bytes),
            sha1: hash(bytes, "sha1"),
            md5_10m: hash(bytes.subarray(0, 10002432)),
          });
          const parts = [{ index: 2, presigned_url: `${url}/part/2` }, {
            index: 1,
            presigned_url: `${url}/part/1`,
          }];
          if (mode === "duplicate") parts[0].index = 1;
          if (mode === "short") parts.pop();
          if (mode === "external") parts[0].presigned_url = "https://example.org/private?secret";
          return Response.json({ upload_id: "resource", block_size: size, parts });
        }
        if (path.endsWith("upload_part_finish")) {
          finishes++;
          const chunk = bytes.subarray((finishes - 1) * size, finishes * size);
          assert.deepEqual(body, {
            upload_id: "resource",
            part_index: finishes,
            block_size: String(chunk.length),
            md5: hash(chunk),
          });
          return mode === "finish"
            ? Response.json({ code: 850012 }, { status: 500 })
            : new Response(null, { status: 204 });
        }
        if (path.endsWith("/files")) {
          merges++;
          assert.deepEqual(body, {
            file_type: 4,
            srv_send_msg: false,
            file_name: "test.zip",
            upload_id: "resource",
          });
          return mode === "merge"
            ? Response.json({ code: 850012 }, { status: 500 })
            : Response.json({ file_info: "uploaded" }, { status: 201 });
        }
        messages++;
        assert.deepEqual(body, { content: "file", msg_type: 7, media: { file_info: "uploaded" } });
        return mode === "unknown"
          ? new Response(null, { status: 500 })
          : Response.json({ id: "sent" });
      },
    );
    const url = `http://127.0.0.1:${server.addr.port}`;
    const client = new QqClient({ appId: "test", appSecret: "secret" }, {
      baseUrl: url,
      uploadTimeoutMs: mode === "timeout" ? 50 : 2000,
      attachmentReader: () => Promise.resolve(bytes),
    });
    try {
      const result = await client.sendText({
        destination: { kind: "user", openId: "target" },
        text: "file",
        attachment: { path: "/test.zip", kind: "file" },
      });
      assert.equal(
        result.status,
        mode === "success" ? "sent" : mode === "unknown" ? "unknown" : "failed",
      );
      assert.equal(messages, mode === "success" || mode === "unknown" ? 1 : 0);
      if (result.status === "failed") {
        assert.equal(result.attempted, false);
        assert.equal(result.error.failureStage, "upload");
        assert.equal(JSON.stringify(result).includes("secret"), false);
      }
      if (["duplicate", "short", "external"].includes(mode)) assert.equal(puts, 0);
      if (mode === "success") assert.deepEqual([puts, finishes, merges, messages], [2, 2, 1, 1]);
    } finally {
      client.close();
      await server.shutdown();
    }
  });
}
