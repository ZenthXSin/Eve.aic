import assert from "node:assert/strict";
import {
  QqPlatformEvents,
  readQqOperatorInteractionTarget,
} from "../adapters/qq/platform-events.ts";
import { inspectQqOperatorEvent } from "../deployment/qq-platform-event-operator.ts";

async function fixture(run: (root: string, directory: string, owner: number) => Promise<void>) {
  const root = await Deno.makeTempDir({ prefix: "eve-qq-receipt-operator-" });
  try {
    const directory = `${root}/platform-events`;
    const inbox = new QqPlatformEvents(directory, { operatorInteractionTargets: true });
    await inbox.initialize();
    await inbox.observe({ op: 0, t: "INTERACTION_CREATE", d: { id: "private-selected-target" } });
    await inbox.observe({ op: 0, t: "GUILD_CREATE", d: { id: "not-a-target" } });
    await inbox.observe({ op: 0, t: "INTERACTION_CREATE", d: { id: "other-private-target" } });
    await run(root, directory, (await Deno.stat(root)).uid!);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

Deno.test("QQ event operator: one canonical integer, fixed root runtime, no listing or path override", async () => {
  const calls: unknown[][] = [];
  const read: typeof readQqOperatorInteractionTarget = (directory, seq, owner) => {
    calls.push([directory, seq, owner]);
    return Promise.resolve(
      seq === 8192
        ? { seq, operation: "acknowledgeInteraction", interactionId: "local-only" }
        : null,
    );
  };
  for (
    const args of [
      [],
      ["0"],
      ["0001"],
      ["+1"],
      ["1.0"],
      [" 1"],
      ["1\n"],
      ["1e1"],
      ["8193"],
      ["1", "2"],
      ["--all"],
      ["--directory", "/tmp"],
      ["../1"],
      ["/root/1"],
      ["--help", "1"],
    ]
  ) {
    await assert.rejects(inspectQqOperatorEvent(args, read, () => 0), TypeError);
  }
  await assert.rejects(inspectQqOperatorEvent(["1"], read, () => 1000));
  assert.deepEqual(calls, []);
  assert.deepEqual(await inspectQqOperatorEvent(["1"], read, () => 0), {
    seq: 1,
    status: "no_target",
  });
  assert.deepEqual(await inspectQqOperatorEvent(["8192"], read, () => 0), {
    seq: 8192,
    operation: "acknowledgeInteraction",
    interactionId: "local-only",
  });
  const fixed = "/root/.local/share/eve-rebuild-runtime/qq/platform-events";
  assert.deepEqual(calls, [[fixed, 1, 0], [fixed, 8192, 0]]);
});

Deno.test("QQ event operator: exact row only while receiver is locked; no writes, scans or repairs", () =>
  fixture(async (root, directory, owner) => {
    using lock = await Deno.open(`${root}/receiver.lock`, {
      createNew: true,
      read: true,
      write: true,
      mode: 0o600,
    });
    assert.equal(await lock.tryLock(true), true);
    await Deno.writeTextFile(`${directory}/00000003.json`, "corrupt evidence");
    const snapshot = () =>
      Array.from(Deno.readDirSync(directory), ({ name }) => {
        const path = `${directory}/${name}`;
        const { ino, mode, mtime } = Deno.statSync(path);
        return { name, ino, mode, mtime, text: Deno.readTextFileSync(path) };
      }).sort((left, right) => left.name.localeCompare(right.name));
    const before = snapshot();
    assert.deepEqual(await readQqOperatorInteractionTarget(directory, 1, owner), {
      seq: 1,
      operation: "acknowledgeInteraction",
      interactionId: "private-selected-target",
    });
    assert.equal(await readQqOperatorInteractionTarget(directory, 2, owner), null);
    for (const seq of [3, 4, 0, -1, 1.5, 8193]) {
      await assert.rejects(readQqOperatorInteractionTarget(directory, seq, owner));
    }
    await assert.rejects(readQqOperatorInteractionTarget(`${root}/missing`, 1, owner));
    assert.deepEqual(snapshot(), before);
    assert.deepEqual(Array.from(Deno.readDirSync(root), ({ name }) => name).sort(), [
      "platform-events",
      "receiver.lock",
    ]);
  }));

Deno.test("QQ event operator: rejects tampered, oversized, linked, exposed or wrong-owner receipts", () =>
  fixture(async (root, directory, owner) => {
    const file = `${directory}/00000001.json`;
    const original = await Deno.readTextFile(file);
    const read = () => readQqOperatorInteractionTarget(directory, 1, owner);
    for (
      const altered of [
        original.replace("private-selected-target", "tampered-target"),
        original.replace('"aliases":[]', '"secret":"do-not-display","aliases":[]'),
        "x".repeat(1281),
      ]
    ) {
      await Deno.writeTextFile(file, altered);
      await assert.rejects(
        read(),
        (error: unknown) => error instanceof Error && !/target|do-not-display/.test(error.message),
      );
      assert.equal(await Deno.readTextFile(file), altered);
    }
    await Deno.writeTextFile(file, original);
    await assert.rejects(readQqOperatorInteractionTarget(directory, 1, owner + 1));
    await Deno.chmod(file, 0o644);
    await assert.rejects(read());
    await Deno.chmod(file, 0o600);
    await Deno.chmod(directory, 0o755);
    await assert.rejects(read());
    await Deno.chmod(directory, 0o700);
    const preserved = `${root}/preserved`;
    await Deno.rename(file, preserved);
    await Deno.symlink(preserved, file);
    await assert.rejects(read());
    await Deno.remove(file);
    await Deno.link(preserved, file);
    await assert.rejects(read());
    assert.equal(await Deno.readTextFile(preserved), original);
    await Deno.symlink(directory, `${root}/link`);
    await assert.rejects(readQqOperatorInteractionTarget(`${root}/link`, 2, owner));
  }));
