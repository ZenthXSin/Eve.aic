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

Deno.test("QQ 附件：共享临时原件可清理，回执保留，显式重下与重启复用兼容", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-qq-tmp-" });
  const directory = `${root}/private`, temporaryDirectory = `${root}/tmp`;
  const owner = await Deno.stat(root);
  const options = { temporaryDirectory, identity: { uid: owner.uid!, gid: owner.gid! } };
  let downloads = new QqPhoneDownloads(directory, undefined, undefined, options);
  const originalFetch = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = () => {
    requests++;
    return Promise.resolve(new Response(png));
  };
  try {
    await downloads.initialize();
    assert.equal(downloads.hasReceipt(source.attachmentId), false);
    assert.equal(await downloads.local(source.attachmentId), undefined);
    const saved = await downloads.download(source, "https://qpic.cn/fixture");
    assert.equal(
      saved.path,
      `${temporaryDirectory}/${createHash("sha256").update(source.attachmentId).digest("hex")}`,
    );
    assert.deepEqual(await downloads.local(source.attachmentId), saved);
    for (const path of [temporaryDirectory, saved.path]) {
      const stat = await Deno.stat(path);
      assert.equal(stat.uid, options.identity.uid);
      assert.equal(stat.gid, options.identity.gid);
      assert.equal(stat.mode! & 0o777, path === temporaryDirectory ? 0o700 : 0o600);
    }
    assert.equal((await Deno.stat(`${directory}/receipts`)).uid, owner.uid);
    await Deno.remove(saved.path);
    assert.equal(await downloads.local(source.attachmentId), undefined);
    assert.equal(downloads.hasReceipt(source.attachmentId), true);
    await downloads.close();
    downloads = new QqPhoneDownloads(directory, undefined, undefined, options);
    await downloads.initialize();
    assert.equal(downloads.hasReceipt(source.attachmentId), true);
    assert.equal(await downloads.local(source.attachmentId), undefined);
    // 普通面板投影和自动预取可凭回执跳过已清理的附件；只有显式下载才重取。
    assert.equal(requests, 1);
    const redownloads = await Promise.all([
      downloads.download(source, "https://qpic.cn/fixture"),
      downloads.download(source, "https://qpic.cn/fixture"),
    ]);
    assert.deepEqual(redownloads, [saved, saved]);
    assert.equal(requests, 2);
    assert.equal(downloads.journal.events.length, 2);
    await downloads.close();
    downloads = new QqPhoneDownloads(directory, undefined, undefined, options);
    await downloads.initialize();
    assert.deepEqual(await downloads.download(source, "https://qpic.cn/expired"), saved);
    assert.equal(requests, 2);
    await Deno.writeFile(saved.path, new Uint8Array(png.length));
    await assert.rejects(
      downloads.download(source, "https://qpic.cn/fixture"),
      (error: unknown) => error instanceof KernelError && error.kind === "corrupt",
    );
    await assert.rejects(
      downloads.image(saved),
      (error: unknown) => error instanceof KernelError && error.kind === "corrupt",
    );
    assert.equal(requests, 2);
    await Deno.remove(saved.path);
    await Deno.symlink(`${directory}/receipts`, saved.path);
    await assert.rejects(
      downloads.local(source.attachmentId),
      (error: unknown) => error instanceof KernelError && error.kind === "corrupt",
    );
  } finally {
    globalThis.fetch = originalFetch;
    await downloads.close();
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("QQ 附件：启用临时目录后旧私有原件仍复用且缺失不重下", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-qq-legacy-" });
  const directory = `${root}/private`, temporaryDirectory = `${root}/tmp`;
  let downloads = new QqPhoneDownloads(directory);
  const originalFetch = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = () => {
    requests++;
    return Promise.resolve(new Response(png));
  };
  try {
    await downloads.initialize();
    const saved = await downloads.download(source, "https://qpic.cn/fixture");
    await downloads.close();
    downloads = new QqPhoneDownloads(directory, undefined, undefined, { temporaryDirectory });
    await downloads.initialize();
    assert.deepEqual(await downloads.local(source.attachmentId), saved);
    assert.deepEqual(await downloads.download(source, "https://qpic.cn/fixture"), saved);
    await Deno.remove(saved.path);
    const corrupt = (error: unknown) => error instanceof KernelError && error.kind === "corrupt";
    await assert.rejects(downloads.local(source.attachmentId), corrupt);
    await assert.rejects(downloads.download(source, "https://qpic.cn/fixture"), corrupt);
    assert.equal(requests, 1);
    assert.equal(downloads.journal.events.length, 1);
  } finally {
    globalThis.fetch = originalFetch;
    await downloads.close();
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("QQ 附件：响应头或分块超限立即取消，不留下临时 part 和虚假回执", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-qq-limit-" });
  const temporaryDirectory = `${root}/tmp`;
  const downloads = new QqPhoneDownloads(`${root}/private`, undefined, undefined, {
    temporaryDirectory,
    maxBytes: 4,
  });
  const originalFetch = globalThis.fetch;
  let cancelled = 0, withLength = true;
  globalThis.fetch = () =>
    Promise.resolve(
      new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(new Uint8Array(5));
          },
          cancel() {
            cancelled++;
          },
        }),
        { headers: withLength ? { "content-length": "5" } : undefined },
      ),
    );
  try {
    await downloads.initialize();
    for (withLength of [true, false]) {
      await assert.rejects(
        downloads.download(source, "https://qpic.cn/fixture"),
        (error: unknown) =>
          rejected(error) &&
          (error as { attachment: { reason: string } }).attachment.reason === "too_large",
      );
      assert.equal(downloads.journal.events.length, 0);
      assert.equal(downloads.hasReceipt(source.attachmentId), false);
      assert.deepEqual(await Array.fromAsync(Deno.readDir(`${temporaryDirectory}/.inflight`)), []);
      assert.deepEqual(
        (await Array.fromAsync(Deno.readDir(temporaryDirectory))).map((entry) => entry.name),
        [".inflight"],
      );
    }
    assert.equal(cancelled, 2);
  } finally {
    globalThis.fetch = originalFetch;
    await downloads.close();
    await Deno.remove(root, { recursive: true });
  }
});

Deno.test("QQ 附件：关闭中断正在下载的临时原件，并等待 part 清理后释放日志", async () => {
  const root = await Deno.makeTempDir({ prefix: "eve-qq-close-" });
  const temporaryDirectory = `${root}/tmp`;
  const downloads = new QqPhoneDownloads(`${root}/private`, undefined, undefined, {
    temporaryDirectory,
  });
  const originalFetch = globalThis.fetch;
  let started!: () => void;
  const firstChunk = new Promise<void>((resolve) => started = resolve);
  globalThis.fetch = (_input, init) =>
    Promise.resolve(
      new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(new Uint8Array([1]));
            init!.signal!.addEventListener("abort", () => controller.error(new Error("aborted")), {
              once: true,
            });
          },
          pull() {
            started();
          },
        }),
      ),
    );
  try {
    await downloads.initialize();
    const pending = downloads.download(source, "https://qpic.cn/fixture");
    const rejectedPending = assert.rejects(
      pending,
      (error: unknown) => error instanceof KernelError && error.kind === "unavailable",
    );
    await firstChunk;
    await downloads.close();
    await rejectedPending;
    assert.deepEqual(await Array.fromAsync(Deno.readDir(`${temporaryDirectory}/.inflight`)), []);
    assert.equal(downloads.journal.events.length, 0);
  } finally {
    globalThis.fetch = originalFetch;
    await downloads.close();
    await Deno.remove(root, { recursive: true });
  }
});
