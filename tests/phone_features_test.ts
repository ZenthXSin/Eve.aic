import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { QqClient } from "../adapters/qq/client.ts";
import { QqSendActions } from "../adapters/qq/actions.ts";
import { QqPhoneDevice } from "../adapters/qq/device.ts";
import { QqPhoneDownloads } from "../adapters/qq/downloads.ts";
import { QqPhoneReading } from "../adapters/qq/reading.ts";
import { QqPhone } from "../adapters/qq/phone.ts";
import { QqTimeline } from "../adapters/qq/timeline.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import { type QqDispatch, qqMessage } from "../adapters/qq/inbound.ts";
import { startPhoneRpc } from "../adapters/qq/rpc.ts";
import { PhoneRpcClient } from "../plugins/phone/client.ts";
import { composePhonePlugin } from "../composition/phone.ts";
import { Kernel } from "../kernel/kernel.ts";
import type { PhoneDownload } from "../contracts/phone.ts";
import type { ModelImage } from "../contracts/model.ts";
import { OwnershipAuthority } from "../guardian/ownership.ts";
import { OwnershipFence } from "../sdk/ownership-fence.ts";
import { ensurePhoneActivationBoundary } from "../adapters/qq/activation.ts";

const KEY = "phone_features_test_connection_0123456789";
const PNG = new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 0]);
async function fixture() {
  const directory = await Deno.makeTempDir({ prefix: "eve-phone-features-" });
  let downloads = 0;
  const requests: { path: string; body: Record<string, unknown> }[] = [];
  let upload: unknown = { file_info: "actual-platform-file-info" };
  let sent: unknown = { id: "actual-platform-message-id" };
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const path = new URL(request.url).pathname;
    if (path === "/asset") {
      downloads++;
      return new Response(PNG);
    }
    if (path === "/redirect") {
      return new Response(null, { status: 302, headers: { location: "http://127.0.0.2/secret" } });
    }
    if (path === "/app/getAppAccessToken") {
      return Response.json({ access_token: "fixture", expires_in: 7200 });
    }
    requests.push({ path, body: await request.json() });
    return Response.json(path.endsWith("/files") ? upload : sent);
  });
  const origin = `http://127.0.0.1:${server.addr.port}`;
  const journal = new QqInboundJournal(`${directory}/incoming`);
  await journal.initialize();
  const actions = new QqSendActions(
    new QqClient({ appId: "fixture", appSecret: "fixture" }, {
      baseUrl: origin,
      attachmentReader: (path) => Deno.readFile(path),
    }),
    `${directory}/actions`,
  );
  await actions.initialize();
  const timeline = new QqTimeline(`${directory}/timeline`);
  await timeline.initialize();
  const reading = new QqPhoneReading(`${directory}/reading`);
  await reading.initialize();
  const device = new QqPhoneDevice(`${directory}/device`);
  await device.initialize();
  const files = new QqPhoneDownloads(`${directory}/downloads`, origin);
  await files.initialize();
  actions.client.useDownloadedAttachmentReader((path) => files.readBytes(path));
  const phone = new QqPhone(
    () => journal.records(),
    actions,
    timeline,
    { id: "bot", displayName: "Eve" },
    reading,
    Date.now,
    device,
    files,
  );
  const rpc = startPhoneRpc(phone, KEY);
  const client = new PhoneRpcClient(rpc.url, KEY);
  async function incoming(seq: number, user: string, asset?: string) {
    const raw: QqDispatch = {
      op: 0,
      s: seq,
      t: "C2C_MESSAGE_CREATE",
      d: {
        id: `message-${seq}`,
        content: "收到的内容",
        author: { user_openid: user },
        ...(asset
          ? {
            attachments: [{
              filename: "../危险名.png",
              content_type: "image/png",
              size: PNG.length,
              url: asset,
            }],
          }
          : {}),
      },
    };
    await journal.append(qqMessage(raw)!, raw);
  }
  return {
    directory,
    origin,
    journal,
    actions,
    timeline,
    reading,
    device,
    files,
    phone,
    client,
    rpc,
    requests,
    incoming,
    downloadCount: () => downloads,
    upload: (value: unknown) => {
      upload = value;
    },
    sent: (value: unknown) => {
      sent = value;
    },
    async close() {
      client.close();
      await rpc.close();
      await actions.close();
      await timeline.close();
      await reading.close();
      await device.close();
      await files.close();
      await server.shutdown();
      await Deno.remove(directory, { recursive: true });
    },
  };
}

Deno.test("手机免打扰、置顶、草稿持久保存；不发送草稿，不标记历史已读", async () => {
  const f = await fixture();
  try {
    for (let seq = 1; seq <= 12; seq++) await f.incoming(seq, `user-${seq}`);
    await f.phone.settings({
      conversationId: "qq:user:user-1",
      muted: true,
      pinned: true,
      draft: "我想说的话，还没发",
    });
    const page = await f.phone.conversations();
    assert.equal(page.items[0].conversationId, "qq:user:user-1");
    assert.deepEqual(page.items[0].settings, { muted: true, pinned: true, hasDraft: true });
    assert.equal(page.items[0].pendingNotifications, 0);
    const page2 = await f.phone.conversations({ cursor: page.older });
    assert.equal(
      new Set([...page.items, ...page2.items].map((item) => item.conversationId)).size,
      12,
    );
    assert.equal(
      (await f.phone.notifications()).items.some((item) =>
        item.handle.conversationId === "qq:user:user-1"
      ),
      false,
    );
    assert.equal((await f.phone.history({ conversationId: "qq:user:user-1" })).messages.length, 1);
    assert.equal(f.reading.get("qq:user:user-1"), null);
    assert.equal(f.requests.length, 0);
    await Promise.all([
      f.phone.settings({ conversationId: "qq:user:user-1", muted: false }),
      f.phone.settings({ conversationId: "qq:user:user-1", pinned: false }),
    ]);
    await f.device.close();
    const restored = new QqPhoneDevice(`${f.directory}/device`);
    await restored.initialize();
    assert.deepEqual(restored.get("qq:user:user-1"), {
      muted: false,
      pinned: false,
      draft: "我想说的话，还没发",
    });
    await restored.close();
  } finally {
    await f.close();
  }
});

Deno.test("附件显式下载；来源、字节哈希与模型图片一致，重启复用原件且不泄漏URL", async () => {
  const f = await fixture();
  try {
    await f.incoming(1, "user-a", `${f.origin}/asset?private-platform-signature=fixture`);
    const screen = await f.phone.open({ conversationId: "qq:user:user-a" });
    assert.equal(f.downloadCount(), 0);
    assert.equal(JSON.stringify(screen).includes("signature"), false);
    const handle = screen.messages[0].handle;
    const index = screen.messages[0].attachments[0].index;
    assert.equal(index, 1);
    const saved = await f.client.call("download", { handle, index }) as unknown as PhoneDownload;
    assert.equal(saved.sha256, createHash("sha256").update(PNG).digest("hex"));
    assert.equal(saved.sizeBytes, PNG.length);
    assert.equal(saved.source.senderPlatformUserId, "user-a");
    assert.equal(saved.path.includes("危险"), false);
    const image = await f.client.call("image", { handle, index }) as unknown as {
      image: ModelImage;
    };
    assert.equal(image.image.provenance.sha256, saved.sha256);
    assert.deepEqual(
      Uint8Array.from(atob(image.image.dataUrl.split(",")[1]), (ch) => ch.charCodeAt(0)),
      PNG,
    );
    assert.equal(f.downloadCount(), 1);
    const filePage = await f.phone.file({
      handle,
      index: 1,
      offsetBytes: 4,
      limitBytes: 4,
      encoding: "base64",
    });
    assert.deepEqual(
      Uint8Array.from(atob(filePage.text), (ch) => ch.charCodeAt(0)),
      PNG.subarray(4, 8),
    );
    assert.equal(filePage.nextOffsetBytes, 8);
    await f.files.close();
    const restored = new QqPhoneDownloads(`${f.directory}/downloads`, f.origin);
    await restored.initialize();
    assert.deepEqual(await restored.download(saved.source, `${f.origin}/asset`), saved);
    assert.equal(f.downloadCount(), 1);
    await Deno.writeTextFile(saved.path, "被修改");
    await assert.rejects(restored.download(saved.source, `${f.origin}/asset`), /变化/);
    await restored.close();
  } finally {
    await f.close();
  }
});

Deno.test("附件拒绝跨来源重定向和同名覆盖，下载失败不假称成功", async () => {
  const f = await fixture();
  try {
    await f.incoming(1, "user-a", `${f.origin}/redirect`);
    const handle = (await f.phone.history({ conversationId: "qq:user:user-a" })).messages[0].handle;
    await assert.rejects(f.phone.download({ handle, index: 1 }), /来源/);
    await assert.rejects(f.phone.download({ handle, index: 0 }));
    await assert.rejects(f.phone.download({ handle, index: 2 }));
    assert.equal(f.files.journal.events.length, 0);
  } finally {
    await f.close();
  }
});

Deno.test("媒体发送先上传再发；只有消息编号算已发送，同动作不会再次上传或发送", async () => {
  const f = await fixture();
  try {
    const path = `${f.directory}/my-image.png`;
    await Deno.writeFile(path, PNG);
    const args = {
      actionId: "send_image_1",
      conversationId: "qq:user:user-a",
      path,
      kind: "image" as const,
    };
    const receipt = await f.phone.sendFile(args);
    assert.equal(receipt.state, "sent");
    assert.equal(f.requests.length, 2);
    assert.equal(f.requests[0].body.file_type, 1);
    assert.equal(f.requests[0].body.srv_send_msg, false);
    assert.equal(f.requests[1].body.msg_type, 7);
    assert.deepEqual(f.requests[1].body.media, { file_info: "actual-platform-file-info" });
    assert.deepEqual(await f.phone.sendFile(args), receipt);
    assert.equal(f.requests.length, 2);
    const history = await f.phone.history({ conversationId: args.conversationId });
    assert.equal(history.messages[0].attachments[0].kind, "image");
    f.sent({});
    assert.equal(
      (await f.phone.sendFile({ ...args, actionId: "send_image_unknown" })).state,
      "unknown",
    );
    assert.equal(
      (await f.phone.sendFile({ ...args, actionId: "send_image_unknown" })).state,
      "unknown",
    );
    assert.equal(f.requests.length, 4);
    f.upload({ err_code: 400 });
    assert.equal(
      (await f.phone.sendFile({ ...args, actionId: "send_image_failed" })).state,
      "failed",
    );
    assert.equal(f.requests.length, 5);
  } finally {
    await f.close();
  }
});

Deno.test("手机新增设备与附件出口拒绝交接后旧号码，不下载、不改草稿、不发文件", async () => {
  const f = await fixture();
  const authority = new OwnershipAuthority(`${f.directory}/authority`, 120_000);
  await authority.initialize();
  const oldKey = "o".repeat(48), newKey = "n".repeat(48);
  await authority.register({ instanceId: "old", sourceCommit: "a".repeat(40), credential: oldKey });
  await authority.register({ instanceId: "new", sourceCommit: "b".repeat(40), credential: newKey });
  const granted = await authority.grant({
    instanceId: "old",
    requestId: "initial",
    expectedEpoch: authority.status().epoch,
  });
  assert.equal(granted.status, "granted");
  if (granted.status !== "granted") throw new Error();
  const oldProof = { instanceId: "old", epoch: granted.owner.epoch, credential: oldKey };
  const fence = new OwnershipFence(`${f.directory}/fence`, "phone", authority);
  await fence.initialize();
  const rpc = startPhoneRpc(f.phone, KEY, undefined, fence);
  const old = new PhoneRpcClient(rpc.url, KEY, 120_000, () => Promise.resolve(oldProof));
  try {
    await f.incoming(1, "user-a", `${f.origin}/asset`);
    const handle = (await f.phone.history({ conversationId: "qq:user:user-a" })).messages[0].handle;
    await old.call("settings", { conversationId: "qq:user:user-a", draft: "旧版本写的草稿" });
    const next = await authority.grant({
      instanceId: "new",
      requestId: "upgrade",
      expectedEpoch: oldProof.epoch,
    });
    assert.equal(next.status, "granted");
    await assert.rejects(
      old.call("settings", { conversationId: "qq:user:user-a", draft: "迟到覆盖" }),
    );
    await assert.rejects(old.call("download", { handle, index: 1 }));
    await assert.rejects(old.call("image", { handle, index: 1 }));
    await assert.rejects(
      old.call("sendFile", {
        actionId: "late_send",
        conversationId: "qq:user:user-a",
        path: "/does-not-exist",
        kind: "image",
      }),
    );
    assert.equal(f.requests.length, 0);
    assert.equal(f.downloadCount(), 0);
    assert.equal(
      (await f.phone.settings({ conversationId: "qq:user:user-a" })).draft,
      "旧版本写的草稿",
    );
  } finally {
    old.close();
    await rpc.close();
    await fence.close();
    await authority.close();
    await f.close();
  }
});

Deno.test("未接低权限读取器时媒体路径读取失败，不向平台上传root可读文件", async () => {
  const f = await fixture();
  try {
    const client = new QqClient({ appId: "fixture", appSecret: "fixture" }, { baseUrl: f.origin });
    const path = `${f.directory}/private-key-fixture`;
    await Deno.writeTextFile(path, "root-only-secret", { mode: 0o600 });
    const result = await client.sendText({
      destination: { kind: "user", openId: "user-a" },
      text: "",
      attachment: { path, kind: "file" },
    });
    assert.equal(result.status, "failed");
    assert.equal(f.requests.length, 0);
    client.close();
  } finally {
    await f.close();
  }
});

Deno.test("手机worker在QQ独立重启后读取新端口，公开连接不能替换密钥或放宽loopback", async () => {
  const old = await fixture(), next = await fixture();
  const kernel = new Kernel({ dataDir: `${old.directory}/kernel-restart` });
  try {
    await old.incoming(1, "before-restart");
    await next.incoming(1, "after-restart");
    const connectionFile = `${old.directory}/public-phone.json`,
      configPath = `${old.directory}/config.json`;
    const keyFile = `${old.directory}/phone-key`;
    await Deno.writeTextFile(keyFile, KEY);
    // fixture只对测试显示同一个固定桥接密钥；正式适配器重启从原bridge-key恢复。
    await Deno.writeTextFile(
      connectionFile,
      JSON.stringify({ phone: { adapterUrl: old.client.url } }),
    );
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        phone: { adapterUrl: old.client.url, adapterKeyFile: keyFile, connectionFile },
      }),
    );
    const pluginDirectory = `${old.directory}/phone-plugin`;
    await composePhonePlugin(pluginDirectory, configPath);
    await kernel.register(pluginDirectory);
    await kernel.start();
    const before = await kernel.call("phone", "conversations", {}) as unknown as {
      items: { conversationId: string }[];
    };
    assert.equal(before.items[0].conversationId, "qq:user:before-restart");
    await old.rpc.close();
    await Deno.writeTextFile(
      connectionFile,
      JSON.stringify({
        phone: { adapterUrl: next.client.url, adapterKeyFile: "/never-read-this-public-value" },
      }),
    );
    const after = await kernel.call("phone", "conversations", {}) as unknown as {
      items: { conversationId: string }[];
    };
    assert.equal(after.items[0].conversationId, "qq:user:after-restart");
    await Deno.writeTextFile(
      connectionFile,
      JSON.stringify({ phone: { adapterUrl: "http://example.org/rpc" } }),
    );
    await assert.rejects(kernel.call("phone", "conversations", {}));
  } finally {
    await kernel.stop();
    await old.close();
    await next.close();
  }
});

Deno.test("手机默认同模型描述，可明确选择原图；描述失败不自动回退原图", async () => {
  const f = await fixture();
  const kernel = new Kernel({ dataDir: `${f.directory}/image-kernel` });
  try {
    await f.incoming(1, "image-sender", `${f.origin}/asset`);
    const handle =
      (await f.phone.history({ conversationId: "qq:user:image-sender" })).messages[0].handle;
    const keyFile = `${f.directory}/key`, configPath = `${f.directory}/config-image.json`;
    await Deno.writeTextFile(keyFile, KEY);
    await Deno.writeTextFile(
      configPath,
      JSON.stringify({
        phone: { adapterUrl: f.client.url, adapterKeyFile: keyFile, describeImages: true },
      }),
    );
    const model = `${f.directory}/model-plugin`;
    await Deno.mkdir(model);
    await Deno.writeTextFile(
      `${model}/manifest.json`,
      JSON.stringify({ name: "model-fixture", version: "1.0.0", provides: { model: "1.0.0" } }),
    );
    await Deno.writeTextFile(
      `${model}/mod.ts`,
      `export default {load(ctx){ ctx.provide("model", {
      describeImage: async(args)=>{const previous=ctx.getState()??{};if(previous.fail)throw Error("description unavailable");await ctx.setState({id:args._actionId,raw:args.image.dataUrl});return {description:"这是一张测试图片的客观描述。"}},
      inspect:()=>ctx.getState()??null,
      fail:async()=>{await ctx.setState({fail:true});return null;}
    });}};`,
    );
    await composePhonePlugin(`${f.directory}/image-phone`, configPath);
    await kernel.register(model);
    await kernel.register(`${f.directory}/image-phone`);
    await kernel.start();
    const seen = await kernel.call("phone", "image", { handle, index: 1 }) as unknown as {
      selectedMode: string;
      description: string;
      provenance: ModelImage["provenance"];
    };
    assert.equal(seen.description, "这是一张测试图片的客观描述。");
    assert.equal(seen.selectedMode, "describe");
    assert.equal(seen.provenance.conversationId, "qq:user:image-sender");
    assert.equal(JSON.stringify(seen).includes("dataUrl"), false);
    assert.equal(JSON.stringify(seen).includes("base64"), false);
    const inspected = await kernel.call("model", "inspect", {}) as { id: string; raw: string };
    assert.match(inspected.id, /^phone_image_[a-f0-9]{64}$/);
    assert.match(inspected.raw, /^data:image\/png;base64,/);
    await kernel.call("phone", "image", { handle, index: 1 });
    assert.equal((await kernel.call("model", "inspect", {}) as { id: string }).id, inspected.id);
    assert.equal(f.downloadCount(), 1);
    await kernel.call("model", "fail", {});
    await assert.rejects(kernel.call("phone", "image", { handle, index: 1 }));
    // 同模型描述出口当前不可用；明确选原图仍可成功，而且不会调用该出口。
    const original = await kernel.call("phone", "image", {
      handle,
      index: 1,
      mode: "original",
    }) as unknown as {
      selectedMode: string;
      image: ModelImage;
      provenance: ModelImage["provenance"];
    };
    assert.equal(original.selectedMode, "original");
    assert.equal(original.image.provenance.sha256, seen.provenance.sha256);
    assert.match(original.image.dataUrl, /^data:image\/png;base64,/);
    assert.equal("description" in original, false);
    await assert.rejects(
      kernel.call("phone", "image", { handle, index: 1, mode: "automatic-fallback" }),
    );
  } finally {
    await kernel.stop();
    await f.close();
  }
});

Deno.test("首次上线捕获真实手机快照边界，旧正文不导入、不伪造已读，重启边界不前移", async () => {
  const f = await fixture();
  try {
    await f.incoming(1, "old-a");
    await f.incoming(2, "old-b");
    await f.phone.open({ conversationId: "qq:user:old-a" });
    await f.phone.send({
      actionId: "old_actual_outbox",
      conversationId: "qq:user:old-a",
      text: "上线前的出站原件",
    });
    const readingBefore = await Deno.readTextFile(`${f.directory}/reading/index.json`);
    const notificationsBefore = await f.phone.notifications();
    const sentBefore = f.requests.length;
    const boundary = await ensurePhoneActivationBoundary(f.phone, f.directory);
    assert.equal(boundary.schema, 1);
    assert.equal(boundary.throughSeq, 3);
    assert.equal(boundary.count, 3);
    assert.equal(boundary.notificationCount, 2);
    assert.match(boundary.digest, /^[a-f0-9]{64}$/);
    const persisted = await Deno.readTextFile(`${f.directory}/activation-boundary.json`);
    assert.equal(persisted.includes("收到的内容"), false);
    assert.equal(persisted.includes("上线前的出站原件"), false);
    assert.equal(await Deno.readTextFile(`${f.directory}/reading/index.json`), readingBefore);
    assert.deepEqual(await f.phone.notifications(), notificationsBefore);
    assert.equal(f.requests.length, sentBefore);
    await f.incoming(3, "new-arrival");
    const recent = await f.phone.notifications({ afterSeq: boundary.throughSeq });
    assert.equal(recent.items.length, 1);
    assert.equal(recent.items[0].handle.conversationId, "qq:user:new-arrival");
    assert.ok(recent.items[0].handle.seq > boundary.throughSeq);
    assert.deepEqual(await ensurePhoneActivationBoundary(f.phone, f.directory), boundary);
    assert.equal(await Deno.readTextFile(`${f.directory}/activation-boundary.json`), persisted);
    assert.equal(f.reading.get("qq:user:old-b"), null);
    assert.equal((await f.phone.history({ conversationId: "qq:user:old-b" })).messages.length, 1);
  } finally {
    await f.close();
  }
});

Deno.test("启动边界损坏保留原件并拒绝覆盖，空快照仍有可验证的零边界", async () => {
  const f = await fixture();
  try {
    const empty = await ensurePhoneActivationBoundary(f.phone, f.directory);
    assert.equal(empty.throughSeq, 0);
    assert.equal(empty.count, 0);
    const path = `${f.directory}/activation-boundary.json`;
    const corrupted = JSON.stringify({ ...empty, digest: "0".repeat(64) });
    await Deno.writeTextFile(path, corrupted);
    await assert.rejects(ensurePhoneActivationBoundary(f.phone, f.directory), /事实来源/);
    assert.equal(await Deno.readTextFile(path), corrupted);
  } finally {
    await f.close();
  }
});
