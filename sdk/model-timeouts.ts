/** Streaming requests have progress deadlines plus a bounded final safety deadline. */
export const MODEL_MAX_DURATION_MS = 30 * 60_000;
// Allow the model worker to cancel I/O and durably save its receipt before an RPC expires.
export const MODEL_RPC_TIMEOUT_MS = MODEL_MAX_DURATION_MS + 30_000;

// Leave time for instance drain, receipt persistence and control shutdown.
export const MODEL_SERVICE_STOP_SECONDS = (MODEL_RPC_TIMEOUT_MS + 90_000) / 1000;
