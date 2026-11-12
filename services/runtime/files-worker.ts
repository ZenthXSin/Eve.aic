import { FileTools } from "./files.ts";
import { toWireError } from "../../kernel/protocol.ts";
import { Buffer } from "node:buffer";

// 此入口必须由宿主以非 root 身份启动；所有请求仅从私有 stdin 读取。
let body = "";
for await (const bytes of Deno.stdin.readable.pipeThrough(new TextDecoderStream())) {
  body += bytes;
  if (body.length > 8_388_608) throw new Error("文件请求过大");
}
try {
  const request = JSON.parse(body);
  if (!["read", "readBytes", "write", "replace", "list", "search"].includes(request.method)) {
    throw new Error("无效文件方法");
  }
  const files = new FileTools(request.root, request.allowedRoots);
  let value;
  if (request.method === "readBytes") {
    const maximum = request.args.maxBytes ?? 20 * 1024 * 1024;
    if (!Number.isSafeInteger(maximum) || maximum < 1 || maximum > 32 * 1024 * 1024) {
      throw new Error("附件读取大小无效");
    }
    const path = await files.path(request.args.path), file = await Deno.open(path);
    try {
      const stat = await file.stat();
      if (!stat.isFile || stat.size > maximum) throw new Error("附件不是普通文件或超过大小限制");
      const bytes = new Uint8Array(maximum + 1);
      let at = 0;
      while (at < bytes.length) {
        const size = await file.read(bytes.subarray(at));
        if (size === null) break;
        at += size;
      }
      if (at > maximum) throw new Error("附件读取期间超过大小限制");
      value = Buffer.from(bytes.subarray(0, at)).toString("base64");
    } finally {
      file.close();
    }
  } else value = await files[request.method as "read"](request.args);
  console.log(JSON.stringify({ value }));
} catch (error) {
  console.log(JSON.stringify({ error: toWireError(error) }));
}
