/** Streaming requests have progress deadlines plus a bounded final safety deadline. */
export const MODEL_MAX_DURATION_MS = 30 * 60_000;
// Allow the model worker to cancel I/O and durably save its receipt before an RPC expires.
export const MODEL_RPC_TIMEOUT_MS = MODEL_MAX_DURATION_MS + 30_000;
