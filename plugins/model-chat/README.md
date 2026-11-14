# Chat Completions transport

Requests use `stream: true` and `stream_options: { include_usage: true }`. The public model contract
still resolves once, after the SSE `[DONE]` marker and complete tool-argument validation. No partial
tool call is executed. Final usage-only chunks are retained, including cache usage; missing usage
remains missing. A compatible provider returning ordinary JSON is still accepted, but its
first-response timeout applies until the complete JSON document arrives.

Timeouts in the private `model` configuration:

| Field                 | Default                   | Meaning                                                                          |
| --------------------- | ------------------------- | -------------------------------------------------------------------------------- |
| `timeoutMs`           | Required existing setting | Fallback for first-token and idle deadlines; no longer a total request timeout   |
| `connectTimeoutMs`    | `timeoutMs`               | Wait for HTTP response headers                                                   |
| `firstTokenTimeoutMs` | `timeoutMs`               | Wait after headers for meaningful model output                                   |
| `idleTimeoutMs`       | `timeoutMs`               | Maximum silence between meaningful deltas, including the final stream terminator |
| `maxDurationMs`       | `1800000`                 | Absolute safety deadline (at most 30 minutes)                                    |

Fetch exposes response headers, rather than TCP/TLS establishment. Some providers flush headers only
with their first token: do not set the header deadline shorter than the permitted initial reasoning
time.

All optional deadlines must be positive integer milliseconds, at most 1800000. Text, refusal,
reasoning and tool-argument fragments count as progress. Comments and empty/role-only events do not
reset progress deadlines. Timeouts remain redacted transport errors, with `timeout: connect`,
`first_token`, `idle`, or `total` identifying the phase. HTTP failures retain their existing
categories. Requests are never automatically replayed after an uncertain outcome.

Model RPCs allow 30 minutes plus 30 seconds for receipt persistence. Graceful shutdown allows that
pending model RPC to finish. Other tool/RPC waits retain their existing limits. Generated
runtime/supervisor systemd units allow 32 minutes for graceful shutdown so the service manager does
not cut off that drain. Close cancels in-flight I/O and releases timers/readers.

Retained text/tool/reasoning bytes use `maxResponseBytes` (default 2 MiB). Individual SSE events are
bounded as well, with a separate larger wire budget for the repeated JSON/SSE framing around
individual tokens.
