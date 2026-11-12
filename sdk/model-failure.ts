import { toWireError } from "../kernel/protocol.ts";

/** These HTTP failures need an explicit operator decision, rather than timer retries. */
export function permanentModelStatus(status: number): boolean {
  return Number.isInteger(status) && status >= 400 && status < 500 &&
    ![408, 409, 425, 429].includes(status);
}

/** Only recognize our fixed, redacted error envelope; never inspect provider response bodies. */
export function providerRejection(error: unknown): number | undefined {
  const message = toWireError(error).message;
  const match = /^(?:ChatModelError: )?模型请求未完成：(provider_rejected|http) \((\d{3})\)$/.exec(
    message,
  );
  if (!match) return;
  const status = Number(match[2]);
  return permanentModelStatus(status) ? status : undefined;
}

export type ModelBlock = {
  category: "provider_rejected";
  httpStatus: number;
  requestId: string;
  at: string;
};
