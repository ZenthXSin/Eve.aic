/** Fixed official SDK protocol source; durable ownership and replay policy stay in Eve. */
export const QQ_OFFICIAL_PROTOCOL = {
  sdk: "@tencent-connect/qqbot-nodejs",
  version: "1.0.5",
  commit: "429a0f1b798f3b0fcef3679e207dbc0469a1df2d",
  source: "https://github.com/tencent-connect/qqbot-nodejs",
} as const;
export const QQ_API_ORIGIN = "https://api.sgroup.qq.com";
export const QQ_TOKEN_ORIGIN = "https://bots.qq.com";
export const QQ_REQUEST_TIMEOUT_MS = 30_000;
export const QQ_UPLOAD_TIMEOUT_MS = 120_000;
export const QQ_TOKEN_TIMEOUT_MS = 10_000;
