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

For a provider confirmed to drop native `tool_calls`, private `model.jsonToolModels` can list exact
model IDs that use the JSON tool protocol. This is opt-in per model, including per-call model
selection; other models, Responses requests and calls without tools keep their existing transport.
The model, credentials, reasoning effort, streaming deadlines and cache session are preserved.

The compatibility request declares the actual tools in a fixed system message and requests
`response_format: { type: "json_object" }`. A fixed final system message repeats the output format
for providers that ignore JSON mode after long histories, preserving the earlier conversation
prefix. Completed assistant content must contain one envelope
`{"content":"...","toolCalls":[{"id":"...","name":"...","args":{}}]}`. If the provider prepends
plain assistant prose, the envelope must start on a new line and consume the entire remaining
content; the prose is preserved as assistant content. Code fences, multiple objects and trailing
explanations are rejected. Existing assistant/tool history is encoded as paired JSON actions and
data receipts without modifying stored messages. Unknown tools, duplicate IDs, malformed arguments,
extra envelope fields, incomplete streams and empty results are rejected. Reasoning text is never
interpreted as an action. Each completion still makes one upstream request; there is no automatic
retry or silent protocol fallback.

The isolated agent service may correct a definitively completed JSON-format rejection once per step
(at most two steps per job), with a new durable request ID and separate budget reservation. When
available, it persists the rejected assistant content as private diagnostic data and includes it in
the correction request; reasoning and simulated receipts are never accepted as results. Correction
requests ask for one action at a time to reduce nesting mistakes, while normal steps retain
multiple-tool support. Existing executed tools and their receipts are preserved on restart.
