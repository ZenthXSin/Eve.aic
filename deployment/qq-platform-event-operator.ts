import { readQqOperatorInteractionTarget } from "../adapters/qq/platform-events.ts";

const inbox = "/root/.local/share/eve-rebuild-runtime/qq/platform-events";

const help = `Usage (local root, private terminal):
  deno run --cached-only --allow-read --allow-sys=uid --deny-net deployment/qq-platform-event-operator.ts SEQ

Read exactly one local inbox sequence (decimal 1..8192), not a QQ gateway sequence.
The path is fixed. No listing, path override, credentials, lock, write, network or automatic acknowledgement.
An INTERACTION_CREATE captured with operatorInteractionTargets: true returns seq, operation and the
authoritative d.id as interactionId. Other valid rows return {seq,status:"no_target"}. Missing,
unsafe or corrupt rows are errors. Keep the ID out of chat, logs and shared terminal transcripts.
An operator target is neither an acknowledgement nor authorization to send one.

Capture is OFF by default. No past targets are reconstructed on startup. On an opted-in redelivery,
only an exact legacy payload digest can upgrade an old metadata-only row; an envelope ID alone does
not prove the original target. Conflicting IDs, missing or unsafe d.id, and excess delivery-key
aliases are rejected without ACK. A receipted row retains up to 8 delivery keys (digests only).

The inbox holds 8192 events; new entries fail at capacity, without eviction. Pages (1..100 items,
default 16) and verified retries remain available. Plan offline maintenance before it fills: stop
the receiver, preserve and verify a private complete backup of the inbox and associated QQ state,
and coordinate consumers' cursors, dedup history and rollback before any operator-led rotation.
Do not delete individual rows, reuse sequence numbers or purge automatically.`;

export async function inspectQqOperatorEvent(
  args: readonly string[],
  read = readQqOperatorInteractionTarget,
  uid = Deno.uid,
) {
  if (args.length !== 1 || !/^[1-9][0-9]{0,3}$/.test(args[0]) || Number(args[0]) > 8192) {
    throw new TypeError("expected exactly one local QQ event sequence (1..8192)");
  }
  if (uid() !== 0) throw new Error("local root required");
  const seq = Number(args[0]);
  const target = await read(inbox, seq, 0);
  return target ?? { seq, status: "no_target" as const };
}

if (import.meta.main) {
  if (Deno.args.length === 1 && Deno.args[0] === "--help") console.log(help);
  else {
    try {
      console.log(JSON.stringify(await inspectQqOperatorEvent(Deno.args)));
    } catch {
      console.error(
        "Cannot inspect this QQ event. Check the local sequence, owner and private store; see --help.",
      );
      Deno.exitCode = 1;
    }
  }
}
