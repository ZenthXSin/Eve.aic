import assert from "node:assert/strict";
import { QqPhoneDownloads } from "../adapters/qq/downloads.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqClient } from "../adapters/qq/client.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { PhoneRpcClient, PhoneRpcError } from "../plugins/phone/client.ts";
import { phoneAttachmentFailure } from "../contracts/phone-rpc.ts";

const key = "attachment_diagnostic_fixture_key_0123456789";
async function fixture(timeoutMs = 30_000) {
  const directory = await Deno.makeTempDir({ prefix: "eve-attachment-error-" });
  const downloads = new QqPhoneDownloads(`${directory}/downloads`, undefined, timeoutMs);
  const timeline = new QqTimeline(`${directory}/timeline`);
  const reading = new QqPhoneReading(`${directory}/reading`);
  const incoming = new QqInboundJournal(`${directory}/incoming`);
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }),
    `${directory}/actions`,
  );
  for (const store of [downloads, timeline, reading, incoming, actions]) await store.initialize();
  const raw: QqDispatch = {
    op: 0,
    s: 1,
    t: "GROUP_MESSAGE_CREATE",
    d: {
      id: "attachment-message",
      group_openid: "fixture",
      author: { member_openid: "sender" },
      content: "图片",
      attachments: [{
        content_type: "image/png",
        filename: "fixture.png",
        url: "https://multimedia.nt.qq.com.cn/download?signature=private-secret",
      }],
    },
  };
  await incoming.append(qqMessage(raw)!, raw);
  const phone = new QqPhone(
    () => incoming.records(),
    actions,
    timeline,
    { id: "fixture-bot", displayName: "Eve" },
    reading,
    Date.now,
    undefined,
    downloads,
  );
  const rpc = startPhoneRpc(phone, key);
  const client = new PhoneRpcClient(rpc.url, key, 2_000);
  return {
    directory,
    downloads,
    client,
    args: { handle: { conversationId: "qq:group:fixture", seq: 1 }, index: 1 },
    async close() {
      client.close();
      await rpc.close();
      for (const store of [downloads, reading, timeline, actions]) await store.close();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

function assertSafe(error: PhoneRpcError) {
  for (const text of [error.message, JSON.stringify(error.attachment)]) {
    assert.equal(text.includes("private-secret"), false);
    assert.equal(text.includes("multimedia.nt.qq.com.cn"), false);
    assert.equal(text.includes("upstream-private-body"), false);
  }
}

Deno.test("QQ 附件：真实手机 RPC 把平台 HTTP 400 安全传到调用方，不降为 request/unknown", async () => {
  const f = await fixture();
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (input, init) =>
    new URL(String(input)).hostname === "multimedia.nt.qq.com.cn"
      ? Promise.resolve(new Response("upstream-private-body", { status: 400 }))
      : originalFetch(input, init);
  try {
    await assert.rejects(f.client.call("image", f.args), (error) => {
      assert.ok(error instanceof PhoneRpcError);
      assert.equal(error.category, "remote");
      assert.equal(error.kind, "remote");
      assert.deepEqual(error.attachment, {
        operation: "attachment_download",
        reason: "http_rejected",
        httpStatus: 400,
      });
      assert.match(error.message, /HTTP 400/);
      assertSafe(error);
      return true;
    });
    assert.equal(f.downloads.journal.events.length, 0);
    assert.deepEqual(await Array.fromAsync(Deno.readDir(`${f.directory}/downloads/files`)), []);
  } finally {
    globalThis.fetch = originalFetch;
    await f.close();
  }
});

Deno.test("QQ 附件：内层期限先于 RPC 终结，网络挂起返回 timeout，未伪造缓存回执", async () => {
  const f = await fixture(20);
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (input, init) => {
    if (new URL(String(input)).hostname !== "multimedia.nt.qq.com.cn") {
      return originalFetch(input, init);
    }
    return new Promise((_resolve, reject) => {
      const signal = init!.signal!;
      if (signal.aborted) reject(signal.reason);
      else signal.addEventListener("abort", () => reject(signal.reason), { once: true });
    });
  };
  try {
    await assert.rejects(f.client.call("image", f.args), (error) => {
      assert.ok(error instanceof PhoneRpcError);
      assert.equal(error.category, "timeout");
      assert.equal(error.kind, "timeout");
      assert.equal(error.attachment?.reason, "timeout");
      assertSafe(error);
      return true;
    });
    assert.equal(f.downloads.journal.events.length, 0);
  } finally {
    globalThis.fetch = originalFetch;
    await f.close();
  }
});

Deno.test("QQ 附件：远端正文中断保持 body_incomplete，部分原件保留而不误报本地存储失败", async () => {
  const f = await fixture();
  const originalFetch = globalThis.fetch;
  globalThis.fetch = (input, init) =>
    new URL(String(input)).hostname === "multimedia.nt.qq.com.cn"
      ? Promise.resolve(
        new Response(
          new ReadableStream({
            start(controller) {
              controller.error(new TypeError("upstream-private-body signature=private-secret"));
            },
          }),
        ),
      )
      : originalFetch(input, init);
  try {
    await assert.rejects(f.client.call("image", f.args), (error) => {
      assert.ok(error instanceof PhoneRpcError);
      assert.equal(error.kind, "remote");
      assert.equal(error.attachment?.reason, "body_incomplete");
      assertSafe(error);
      return true;
    });
    assert.equal(f.downloads.journal.events.length, 0);
    const files = await Array.fromAsync(Deno.readDir(`${f.directory}/downloads/files`));
    assert.equal(files.length, 1);
    assert.equal(files[0].name.endsWith(".part"), true);
  } finally {
    globalThis.fetch = originalFetch;
    await f.close();
  }
});

Deno.test("QQ 附件：RPC 诊断严格筛选，不透传扩展字段或伪装原因", () => {
  for (
    const diagnostic of [
      {
        operation: "attachment_download",
        reason: "http_rejected",
        httpStatus: 400,
        url: "private",
      },
      { operation: "attachment_download", reason: "https://private-secret" },
      { operation: "attachment_download", reason: "http_rejected", httpStatus: "private-secret" },
      { operation: "attachment_download", reason: { toString: () => "http_rejected" } },
    ]
  ) {
    assert.equal(phoneAttachmentFailure(diagnostic), undefined);
    assert.equal(new PhoneRpcError("remote", diagnostic).message, "手机请求未完成：remote");
  }
});
