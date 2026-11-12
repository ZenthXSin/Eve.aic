import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { QqPhoneDownloads } from "../adapters/qq/downloads.ts";
import { KernelError } from "../kernel/protocol.ts";

const source = {
  kind: "phone_attachment" as const,
  conversationId: "qq:group:fixture",
  sourceSeq: 74,
  attachmentId: "qq:group:fixture:74:1",
  senderPlatformUserId: "fixture-sender",
};
const png = Uint8Array.from(
  atob(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a5ioAAAAASUVORK5CYII=",
  ),
  (character) => character.charCodeAt(0),
);
const rejected = (error: unknown) => error instanceof KernelError && error.kind === "rejected";

Deno.test("QQ 附件：真实多媒体主机可下载识图，持久化回执复用，不泄露带签名资源地址", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-download-" });
  let downloads = new QqPhoneDownloads(directory);
  const originalFetch = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = (input, init) => {
    requests++;
    assert.equal(new URL(String(input)).hostname, "multimedia.nt.qq.com.cn");
    assert.equal(init?.redirect, "manual");
    return Promise.resolve(new Response(png, { headers: { "content-type": "image/png" } }));
  };
  try {
    await downloads.initialize();
    const saved = await downloads.download(
      source,
      "https://multimedia.nt.qq.com.cn/download?fixture-signature=private",
    );
    assert.equal(saved.sizeBytes, png.length);
    assert.equal(saved.sha256, createHash("sha256").update(png).digest("hex"));
    assert.equal(saved.mimeType, "image/png");
    assert.equal(JSON.stringify(saved).includes("fixture-signature"), false);
    const viewed = await downloads.image(saved);
    assert.equal(viewed.image.dataUrl.startsWith("data:image/png;base64,"), true);
    assert.deepEqual(viewed.image.provenance.sha256, saved.sha256);
    await downloads.close();
    downloads = new QqPhoneDownloads(directory);
    await downloads.initialize();
    assert.deepEqual(
      await downloads.download(source, "https://multimedia.nt.qq.com.cn/download?expired=1"),
      saved,
    );
    assert.equal(requests, 1);
  } finally {
    globalThis.fetch = originalFetch;
    await downloads.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("QQ 附件：补充精确主机后仍拒绝相似域名、凭据、非 HTTPS 及跨来源重定向", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-qq-download-" });
  const downloads = new QqPhoneDownloads(directory);
  const originalFetch = globalThis.fetch;
  let requests = 0;
  let location = "https://evil.example/secret";
  globalThis.fetch = () => {
    requests++;
    return Promise.resolve(new Response(null, { status: 302, headers: { location } }));
  };
  try {
    await downloads.initialize();
    for (
      const address of [
        "http://multimedia.nt.qq.com.cn/download",
        "https://multimedia.nt.qq.com.cn:444/download",
        "https://multimedia.nt.qq.com.cn.evil.example/download",
        "https://evil.multimedia.nt.qq.com.cn/download",
        "https://another.qq.com.cn/download",
        "https://user:pass@multimedia.nt.qq.com.cn/download",
        "https://multimedia.nt.qq.com.cn/download#fragment",
      ]
    ) await assert.rejects(downloads.download(source, address), rejected);
    assert.equal(requests, 0);
    for (
      location of [
        "https://evil.example/download",
        "http://multimedia.nt.qq.com.cn/download",
        "https://multimedia.nt.qq.com.cn:444/download",
        "https://another.qq.com.cn/download",
      ]
    ) {
      const before: number = requests;
      await assert.rejects(
        downloads.download(source, "https://multimedia.nt.qq.com.cn/download"),
        rejected,
      );
      assert.equal(requests, before + 1);
    }
    assert.equal(downloads.journal.events.length, 0);
    assert.deepEqual(await Array.fromAsync(Deno.readDir(`${directory}/files`)), []);
  } finally {
    globalThis.fetch = originalFetch;
    await downloads.close();
    await Deno.remove(directory, { recursive: true });
  }
});
