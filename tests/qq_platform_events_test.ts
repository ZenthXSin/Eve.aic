import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { type QqInboundEvent, qqMessage } from "../adapters/qq/inbound.ts";
import { QqInboundJournal } from "../adapters/qq/journal.ts";
import {
  QqPlatformEvents,
  type QqPlatformEventsOptions,
  readQqOperatorInteractionTarget,
} from "../adapters/qq/platform-events.ts";
import type { PhonePlatformEventsArgs } from "../contracts/phone-platform.ts";
import { KernelError } from "../kernel/protocol.ts";

async function temporary(run: (root: string) => Promise<void>) {
  const root = await Deno.makeTempDir({ prefix: "eve-qq-platform-events-" });
  try {
    await run(root);
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

async function start(directory: string, options?: QqPlatformEventsOptions) {
  const inbox = new QqPlatformEvents(directory, options);
  assert.equal(inbox.available, false);
  await inbox.initialize();
  return inbox;
}

function event(type: string, id?: string, data: unknown = {}): QqInboundEvent {
  return { op: 0, t: type, ...(id === undefined ? {} : { id }), d: data };
}

const failure = (kind: KernelError["kind"]) => (error: unknown) =>
  error instanceof KernelError && error.kind === kind;

function names(path: string) {
  return Array.from(Deno.readDirSync(path), (entry) => entry.name).sort();
}

const optIn = { operatorInteractionTargets: true };

function interaction(id: string, envelope?: string, fields: Record<string, unknown> = {}) {
  return event("INTERACTION_CREATE", envelope, { ...fields, id });
}

async function receipt(directory: string, seq: number) {
  return await readQqOperatorInteractionTarget(directory, seq, (await Deno.stat(directory)).uid!);
}

Deno.test("QQ interaction receipts: opt-in captures only d.id; never in phone pages", () =>
  temporary(async (root) => {
    const target = "callback.A_1-+=~";
    const fields = {
      data: { resolved: { button_id: "private-button", button_data: "secret-action-key" } },
      user_openid: "secret-user-openid",
      message_id: "private-message-id",
      content: "private-message-body",
      application_id: "private-app-id",
    };
    const delivery = interaction(target, "secret-envelope-id", fields);
    const disabled = await start(`${root}/off`);
    await disabled.observe(delivery);
    assert.equal(await receipt(`${root}/off`, 1), null);
    assert.ok(!(await Deno.readTextFile(`${root}/off/00000001.json`)).includes(target));

    const directory = `${root}/on`;
    const enabled = await start(directory, optIn);
    await enabled.observe(delivery);
    await enabled.observe(
      event("MESSAGE_REACTION_ADD", "another-envelope", { ...fields, id: target }),
    );
    assert.deepEqual(await receipt(directory, 1), {
      seq: 1,
      operation: "acknowledgeInteraction",
      interactionId: target,
    });
    assert.equal(await receipt(directory, 2), null);
    for (const [index, name] of names(directory).entries()) {
      const text = await Deno.readTextFile(`${directory}/${name}`);
      assert.equal(text.includes(target), index === 0);
      assert.equal((await Deno.stat(`${directory}/${name}`)).mode! & 0o777, 0o600);
      for (
        const neverPersist of [
          "data",
          "resolved",
          "button_id",
          "button_data",
          "secret-action-key",
          "private-button",
          "secret-envelope-id",
          "another-envelope",
          ...Object.keys(fields),
          ...Object.values(fields).filter((value) => typeof value === "string"),
        ]
      ) {
        assert.ok(!text.includes(neverPersist));
      }
    }
    const page = await enabled.page({});
    assert.ok(!JSON.stringify(page).includes(target));
    for (const item of page.items) {
      assert.deepEqual(Object.keys(item).sort(), ["receivedAt", "seq", "type"]);
    }
  }));

Deno.test("QQ interaction receipts: missing, nested, inherited and unsafe targets cannot be ACKed", () =>
  temporary(async (directory) => {
    const inbox = await start(directory, optIn);
    const notTheTarget = "do-not-substitute-envelope";
    for (
      const data of [
        null,
        [],
        { interaction_id: notTheTarget },
        { data: { id: notTheTarget } },
        Object.create({ id: notTheTarget }),
        {
          get id() {
            return assert.fail("getter executed");
          },
        },
        ...[
          null,
          1,
          {},
          "",
          "..",
          "___",
          "user/key",
          "a\n",
          "a\0",
          "signed?token",
          "https://private.invalid/",
          "x".repeat(257),
        ].map((id) => ({ id })),
      ]
    ) {
      await assert.rejects(
        inbox.observe(event("INTERACTION_CREATE", notTheTarget, data)),
        (error: unknown) => error instanceof TypeError && !error.message.includes(notTheTarget),
      );
    }
    assert.deepEqual(names(directory), []);
    assert.equal(inbox.available, true);
    await inbox.observe(interaction("x".repeat(256), notTheTarget));
    assert.equal((await receipt(directory, 1))?.interactionId.length, 256);
  }));

Deno.test("QQ interaction receipts: changed deliveries converge across restart; conflicting targets never overwrite", () =>
  temporary(async (directory) => {
    const inbox = await start(directory, optIn);
    const target = "first-interaction", other = "second-interaction";
    await Promise.all([
      inbox.observe(interaction(target)),
      inbox.observe(interaction(target, "envelope-a", { changed: true })),
      inbox.observe({ ...interaction(target, "envelope-b"), s: 2345 }),
      inbox.observe(interaction(other, "envelope-c")),
    ]);
    const before = await inbox.page({});
    assert.deepEqual(before.items.map(({ seq }) => seq), [1, 2]);
    const restart = await start(directory, optIn);
    await restart.observe(interaction(target, undefined, { changed: true }));
    await restart.observe(interaction(target, "envelope-a"));
    await restart.observe(interaction(target, "envelope-d"));
    const files = names(directory).map((name) => Deno.readTextFileSync(`${directory}/${name}`));
    for (
      const invalid of [
        interaction("unseen-interaction", "envelope-a"),
        interaction(other, "envelope-b"),
        interaction(target, "envelope-c"),
      ]
    ) {
      await assert.rejects(
        restart.observe(invalid),
        (error: unknown) =>
          failure("rejected")(error) && !String(error).includes("interaction") &&
          !String(error).includes("envelope"),
      );
    }
    assert.deepEqual(
      names(directory).map((name) => Deno.readTextFileSync(`${directory}/${name}`)),
      files,
    );
    assert.deepEqual(await restart.page({}), before);
    assert.equal((await receipt(directory, 1))?.interactionId, target);
    assert.equal((await receipt(directory, 2))?.interactionId, other);

    const disabled = await start(directory);
    await disabled.observe(interaction(target, "envelope-d"));
    await assert.rejects(disabled.observe(interaction(other, "envelope-d")), failure("rejected"));
    await disabled.observe(interaction("not-captured", "envelope-e"));
    assert.equal(await receipt(directory, 3), null);
  }));

Deno.test("QQ interaction receipts: bounded delivery aliases fail before ACK and cannot be silently forgotten", () =>
  temporary(async (directory) => {
    const inbox = await start(directory, optIn);
    const target = "z".repeat(256);
    for (let index = 1; index <= 8; index++) {
      await inbox.observe(interaction(target, `delivery-${index}`));
    }
    const before = await Deno.readTextFile(`${directory}/00000001.json`);
    const restart = await start(directory, optIn);
    await assert.rejects(restart.observe(interaction(target, "delivery-9")), failure("rejected"));
    for (let index = 1; index <= 8; index++) {
      await restart.observe(interaction(target, `delivery-${index}`));
      await assert.rejects(
        restart.observe(interaction("different", `delivery-${index}`)),
        failure("rejected"),
      );
    }
    assert.equal(await Deno.readTextFile(`${directory}/00000001.json`), before);
    assert.deepEqual(names(directory), ["00000001.json"]);
  }));

Deno.test("QQ interaction receipts: enabling does not guess historical targets from envelope IDs", () =>
  temporary(async (directory) => {
    const disabled = await start(directory);
    const idless = interaction("verifiable", undefined, { other: 1 });
    await disabled.observe(idless);
    await disabled.observe(interaction("ambiguous", "old-envelope"));
    await disabled.observe(interaction("new-envelope-on-retry", undefined, { other: 2 }));
    const metadata = await disabled.page({});
    const restart = await start(directory, optIn);
    assert.equal(await receipt(directory, 1), null);
    assert.equal(await receipt(directory, 2), null);
    const untouched = await Deno.readTextFile(`${directory}/00000002.json`);
    for (const candidate of ["ambiguous", "imposter"]) {
      await assert.rejects(
        restart.observe(interaction(candidate, "old-envelope")),
        failure("rejected"),
      );
    }
    assert.equal(await Deno.readTextFile(`${directory}/00000002.json`), untouched);
    await restart.observe({ ...idless, d: { id: "verifiable", other: 1 } });
    assert.equal((await receipt(directory, 1))?.interactionId, "verifiable");
    await restart.observe(interaction("new-envelope-on-retry", "new-envelope", { other: 2 }));
    assert.equal((await receipt(directory, 3))?.interactionId, "new-envelope-on-retry");
    await assert.rejects(
      (await start(directory, optIn)).observe(interaction("imposter", "new-envelope")),
      failure("rejected"),
    );
    assert.deepEqual(await restart.page({}), metadata);
  }));

Deno.test("QQ interaction receipts: refuse unsafe ancestors and foreign ownership before the first write", () =>
  temporary(async (root) => {
    const parent = `${root}/qq`, directory = `${parent}/platform-events`;
    await Deno.mkdir(parent, { mode: 0o700 });
    await Deno.chmod(parent, 0o777);
    await assert.rejects(start(directory, optIn), failure("corrupt"));
    assert.deepEqual(names(parent), []);
    await Deno.chmod(parent, 0o700);
    const inbox = await start(directory, optIn);
    await Deno.chmod(parent, 0o777);
    await assert.rejects(inbox.observe(interaction("not-written")), failure("corrupt"));
    assert.deepEqual(names(directory), []);
    await Deno.chmod(parent, 0o700);
    if ((await Deno.stat(root)).uid === 0) {
      await Deno.chown(directory, 65534, null);
      await assert.rejects(start(directory, optIn), failure("corrupt"));
      assert.deepEqual(names(directory), []);
    }
  }));

Deno.test("QQ platform inbox: only safe metadata; chat, session and journal records are not duplicated", () =>
  temporary(async (root) => {
    const directory = `${root}/qq/platform-events`;
    const inbox = await start(directory);
    assert.equal(inbox.available, true);
    assert.equal((await Deno.stat(directory)).mode! & 0o777, 0o700);

    const privateFields = {
      auth_token: "secret-token",
      url: "https://private.invalid/signed",
      content: "private body",
      timestamp: "attacker time",
    };
    const chat = event("C2C_MESSAGE_CREATE", "secret-id", {
      ...privateFields,
      id: "message-id",
      author: { user_openid: "secret-user" },
    });
    const journal = new QqInboundJournal(`${root}/qq/inbound`);
    await journal.initialize();
    const { record } = await journal.append(qqMessage(chat)!, chat);
    await inbox.observe(chat, record);
    await inbox.observe(event("NEW_CHAT_KIND", "new-id", chat.d), record);
    for (
      const type of [
        "READY",
        "RESUMED",
        "MESSAGE_CREATE",
        "AT_MESSAGE_CREATE",
        "DIRECT_MESSAGE_CREATE",
        "C2C_MESSAGE_CREATE",
        "GROUP_AT_MESSAGE_CREATE",
        "GROUP_MESSAGE_CREATE",
      ]
    ) {
      await inbox.observe({ ...chat, t: type });
    }
    assert.deepEqual(names(directory), []);
    assert.equal(journal.records().length, 1);

    await inbox.observe(event("INTERACTION_CREATE", "secret-id", privateFields));
    await inbox.observe(event("UNRECOGNIZED_PRIVATE_TYPE", undefined, { nested: [privateFields] }));
    const result = await inbox.page({});
    assert.deepEqual(result.items.map(({ seq, type }) => [seq, type]), [[1, "INTERACTION_CREATE"], [
      2,
      "UNKNOWN",
    ]]);
    for (const item of result.items) {
      assert.deepEqual(Object.keys(item).sort(), ["receivedAt", "seq", "type"]);
      assert.equal(new Date(item.receivedAt).toISOString(), item.receivedAt);
    }
    for (const name of names(directory)) {
      const stat = await Deno.lstat(`${directory}/${name}`);
      assert.equal(stat.mode! & 0o777, 0o600);
      const text = await Deno.readTextFile(`${directory}/${name}`);
      for (
        const sensitive of [
          "secret-id",
          "UNRECOGNIZED_PRIVATE_TYPE",
          ...Object.keys(privateFields),
          ...Object.values(privateFields),
        ]
      ) {
        assert.ok(!text.includes(sensitive) && !name.includes(sensitive));
      }
    }
  }));

Deno.test("QQ platform inbox: stable dispatch IDs and canonical id-less retries survive restart", () =>
  temporary(async (directory) => {
    const inbox = await start(directory);
    const explicit = event("FRIEND_ADD", "dispatch-1", { user_openid: "abc" });
    const body = { id: "resource-id", data: { members: ["a", "b"], n: 3 } };
    const idless = event("GUILD_UPDATE", undefined, body);
    const first = inbox.observe({ ...explicit, s: 99 });
    const second = inbox.observe(idless);
    body.data.members.reverse();
    idless.t = "CHANNEL_DELETE";
    const reordered = event("GUILD_UPDATE", undefined, {
      data: { n: 3, members: ["a", "b"] },
      id: "resource-id",
    });
    await Promise.all([
      first,
      second,
      inbox.observe({ ...explicit, s: 100, d: { changed: "delivery fields" } }),
      inbox.observe({ ...reordered, s: 99 }),
      inbox.observe(event("GUILD_UPDATE", undefined, body)),
      inbox.observe(event("FRIEND_ADD", "dispatch-2", explicit.d)),
    ]);
    const before = await inbox.page({});
    assert.deepEqual(before.items.map(({ type }) => type), [
      "FRIEND_ADD",
      "GUILD_UPDATE",
      "GUILD_UPDATE",
      "FRIEND_ADD",
    ]);
    const files = names(directory).map((name) => Deno.readTextFileSync(`${directory}/${name}`));
    const restart = await start(directory);
    await Promise.all([
      restart.observe(explicit),
      restart.observe(reordered),
      restart.observe(event("GUILD_UPDATE", undefined, body)),
    ]);
    assert.deepEqual(await restart.page({}), before);
    assert.deepEqual(
      names(directory).map((name) => Deno.readTextFileSync(`${directory}/${name}`)),
      files,
    );
    await restart.observe(event("FRIEND_DEL", "next"));
    assert.equal((await restart.page({ afterSeq: 4 })).items[0].seq, 5);
  }));

Deno.test("QQ platform inbox: exclusive cursor, serial pages, stable copies and bounded arguments", () =>
  temporary(async (directory) => {
    const inbox = await start(directory);
    assert.deepEqual(await inbox.page({}), { items: [], throughSeq: 0, hasMore: false });
    const writes = Array.from(
      { length: 18 },
      (_, index) => inbox.observe(event("FRIEND_ADD", String(index))),
    );
    const snapshot = inbox.page({});
    const beforeLast = inbox.page({ afterSeq: 18 });
    const last = inbox.observe(event("FRIEND_DEL", "last"));
    await Promise.all([...writes, last]);
    const first = await snapshot;
    assert.deepEqual(await beforeLast, { items: [], throughSeq: 18, hasMore: false });
    assert.deepEqual(
      first.items.map(({ seq }) => seq),
      Array.from({ length: 16 }, (_, index) => index + 1),
    );
    assert.equal(first.throughSeq, 16);
    assert.equal(first.hasMore, true);
    first.items[0].type = "changed";
    assert.equal((await inbox.page({ limit: 1 })).items[0].type, "FRIEND_ADD");
    const next = await inbox.page({ afterSeq: first.throughSeq, limit: 2 });
    assert.deepEqual(next.items.map(({ seq }) => seq), [17, 18]);
    assert.equal(next.throughSeq, 18);
    assert.equal(next.hasMore, true);
    const end = await inbox.page({ afterSeq: next.throughSeq, limit: 100 });
    assert.deepEqual(end.items.map(({ seq }) => seq), [19]);
    assert.equal(end.throughSeq, 19);
    assert.equal(end.hasMore, false);
    assert.deepEqual(await inbox.page({ afterSeq: end.throughSeq }), {
      items: [],
      throughSeq: 19,
      hasMore: false,
    });
    assert.deepEqual(await inbox.page({ afterSeq: 16, limit: 2 }), next);
    for (
      const args of [
        null,
        [],
        "1",
        { extra: true },
        { afterSeq: 20 },
        { afterSeq: -1 },
        { afterSeq: 1.5 },
        { afterSeq: null },
        { afterSeq: Number.MAX_SAFE_INTEGER + 1 },
        { limit: NaN },
        { limit: 0 },
        { limit: 101 },
        { limit: "5" },
      ]
    ) {
      await assert.rejects(inbox.page(args as PhonePlatformEventsArgs), TypeError);
    }
    assert.equal(inbox.available, true);
  }));

Deno.test("QQ platform inbox: observe, duplicate and page wait for file and directory fsync", () =>
  temporary(async (directory) => {
    const inbox = await start(directory, optIn);
    const file = { entered: Promise.withResolvers<void>(), release: Promise.withResolvers<void>() };
    const dir = { entered: Promise.withResolvers<void>(), release: Promise.withResolvers<void>() };
    const original = Deno.FsFile.prototype.sync;
    Deno.FsFile.prototype.sync = async function () {
      const gate = (await this.stat()).isDirectory ? dir : file;
      gate.entered.resolve();
      await gate.release.promise;
      await original.call(this);
    };
    const finished: string[] = [];
    const delivery = interaction("durable-target", "durable");
    const work = Promise.all([
      inbox.observe(delivery).then(() => finished.push("observe")),
      inbox.observe(delivery).then(() => finished.push("duplicate")),
      inbox.page({}).then((page) => {
        assert.equal(page.throughSeq, 1);
        finished.push("page");
      }),
    ]);
    try {
      for (const gate of [file, dir]) {
        await Promise.race([
          gate.entered.promise,
          work.then(() => assert.fail("resolved before sync")),
        ]);
        assert.deepEqual(finished, []);
        gate.release.resolve();
      }
      await work;
    } finally {
      Deno.FsFile.prototype.sync = original;
      file.release.resolve();
      dir.release.resolve();
      await Promise.allSettled([work]);
    }
    assert.deepEqual(finished, ["observe", "duplicate", "page"]);
    assert.equal((await (await start(directory)).page({})).throughSeq, 1);
    assert.equal((await receipt(directory, 1))?.interactionId, "durable-target");
  }));

Deno.test("QQ platform inbox: an uncertain commit never ACKs, including queued retries; restart can verify it", () =>
  temporary(async (directory) => {
    const inbox = await start(directory, optIn);
    await inbox.observe(interaction("retained-target", "original"));
    const delivery = interaction("retained-target", "uncertain-alias");
    const original = Deno.FsFile.prototype.sync;
    Deno.FsFile.prototype.sync = async function () {
      if ((await this.stat()).isDirectory) throw new Error("injected directory sync failure");
      await original.call(this);
    };
    try {
      await Promise.all([
        assert.rejects(inbox.observe(delivery), failure("storage")),
        assert.rejects(inbox.observe(delivery), failure("unavailable")),
        assert.rejects(inbox.page({}), failure("unavailable")),
      ]);
    } finally {
      Deno.FsFile.prototype.sync = original;
    }
    assert.equal(inbox.available, false);
    await assert.rejects(inbox.observe(delivery), failure("unavailable"));
    const restart = await start(directory, optIn);
    await restart.observe(delivery);
    await assert.rejects(
      restart.observe(interaction("wrong-target", "uncertain-alias")),
      failure("rejected"),
    );
    assert.equal((await receipt(directory, 1))?.interactionId, "retained-target");
    assert.equal((await restart.page({})).throughSeq, 1);
    const [name] = names(directory);
    await Deno.remove(`${directory}/${name}`);
    await assert.rejects(restart.observe(delivery));
    assert.equal(restart.available, false);
  }));

Deno.test("QQ platform inbox: admission bounds do not consume a seq or poison the durable retry index", () =>
  temporary(async (directory) => {
    const inbox = new QqPlatformEvents(directory);
    const delivery = event("FRIEND_ADD", "concurrent");
    await assert.rejects(inbox.observe(delivery), failure("unavailable"));
    await assert.rejects(inbox.page({}), failure("unavailable"));
    await inbox.initialize();
    await assert.rejects(inbox.initialize(), failure("unavailable"));
    const cyclic: unknown[] = [];
    cyclic.push(cyclic);
    for (
      const bad of [
        event("", "id"),
        event("x".repeat(129)),
        event("FRIEND_ADD", "x".repeat(513)),
        event("FRIEND_ADD", ""),
        event("FRIEND_ADD", undefined, "x".repeat(1_048_577)),
        event("FRIEND_ADD", undefined, Array(16_385).fill(0)),
        event("FRIEND_ADD", undefined, cyclic),
      ]
    ) {
      await assert.rejects(inbox.observe(bad), TypeError);
    }
    const burst = Array.from({ length: 256 }, () => inbox.observe(delivery));
    await assert.rejects(inbox.observe(event("FRIEND_DEL", "overload")), failure("unavailable"));
    await Promise.all(burst);
    await inbox.observe(event("FRIEND_DEL", "overload"));
    assert.deepEqual((await inbox.page({})).items.map(({ seq, type }) => [seq, type]), [[
      1,
      "FRIEND_ADD",
    ], [2, "FRIEND_DEL"]]);
  }));

function encode(body: Record<string, unknown>) {
  const checksum = createHash("sha256").update(JSON.stringify(body)).digest("hex");
  return JSON.stringify({ ...body, checksum }) + "\n";
}

Deno.test("QQ platform inbox: corruption and unfinished writes fail closed without modifying evidence", () =>
  temporary(async (root) => {
    const directory = `${root}/inbox`;
    const inbox = await start(directory);
    await inbox.observe(event("FRIEND_ADD", "first"));
    const file = `${directory}/00000001.json`;
    const original = await Deno.readTextFile(file);
    const { checksum: _checksum, ...body } = JSON.parse(original);
    for (
      const text of [
        original.slice(0, -4),
        "x".repeat(513),
        original.replace("FRIEND_ADD", "FRIEND_DEL"),
        encode({ ...body, schema: 2 }),
        encode({ ...body, type: "RAW_PRIVATE_NAME" }),
        encode({ ...body, receivedAt: "bad date" }),
        encode({ ...body, seq: 0 }),
        encode({ ...body, key: "raw identifier" }),
        encode({ ...body, secret: "unexpected field" }),
      ]
    ) {
      await Deno.writeTextFile(file, text);
      const failed = new QqPlatformEvents(directory);
      await assert.rejects(failed.initialize(), failure("corrupt"));
      assert.equal(failed.available, false);
      await assert.rejects(failed.page({}), failure("unavailable"));
      await assert.rejects(failed.observe(event("FRIEND_ADD", "later")), failure("unavailable"));
      assert.equal(await Deno.readTextFile(file), text);
    }
    await Deno.writeTextFile(file, original);
    for (
      const [name, text] of [["00000002.json", encode({ ...body, seq: 2 })], [
        "00000003.json",
        original,
      ], ["00000002.json.partial.tmp", "unfinished"]]
    ) {
      const path = `${directory}/${name}`;
      await Deno.writeTextFile(path, text, { mode: 0o600 });
      await assert.rejects(new QqPlatformEvents(directory).initialize(), failure("corrupt"));
      assert.equal(await Deno.readTextFile(path), text);
      assert.equal(await Deno.readTextFile(file), original);
      await Deno.remove(path);
    }
  }));

Deno.test("QQ platform inbox: refuses symlinks, hard-linked records, unsafe modes and path substitution", () =>
  temporary(async (root) => {
    const directory = `${root}/inbox`, outside = `${root}/outside`, alias = `${root}/alias`;
    await Deno.mkdir(outside, { mode: 0o700 });
    await Deno.symlink(outside, alias);
    await assert.rejects(start(alias));
    await assert.rejects(start(`${alias}/missing`));
    assert.deepEqual(names(outside), []);
    const inbox = await start(directory);
    await inbox.observe(event("FRIEND_ADD", "first"));
    const file = `${directory}/00000001.json`, privateFile = `${outside}/untouched.json`;
    await Deno.rename(file, privateFile);
    const original = await Deno.readTextFile(privateFile);
    await Deno.symlink(privateFile, file);
    await assert.rejects(start(directory));
    await Deno.remove(file);
    await Deno.link(privateFile, file);
    await assert.rejects(start(directory));
    await Deno.remove(file);
    assert.equal(await Deno.readTextFile(privateFile), original);
    await Deno.writeTextFile(file, original, { mode: 0o644 });
    await Deno.chmod(file, 0o644);
    await assert.rejects(start(directory));
    await Deno.chmod(file, 0o600);
    await Deno.chmod(directory, 0o755);
    await assert.rejects(start(directory));
    await Deno.chmod(directory, 0o700);
    await Deno.rename(directory, `${root}/moved`);
    await Deno.symlink(outside, directory);
    await assert.rejects(inbox.observe(event("FRIEND_ADD", "second")), failure("corrupt"));
    assert.equal(inbox.available, false);
    assert.deepEqual(names(outside), ["untouched.json"]);
    assert.equal(await Deno.readTextFile(privateFile), original);
  }));

Deno.test("QQ platform inbox: full storage rejects new facts but preserves pages and durable duplicates", () =>
  temporary(async (directory) => {
    const first = await start(directory);
    const delivery = event("FRIEND_ADD", "retained");
    await first.observe(delivery);
    const { checksum: _checksum, ...body } = JSON.parse(
      await Deno.readTextFile(`${directory}/00000001.json`),
    );
    for (let seq = 2; seq < 8192; seq++) {
      Deno.writeTextFileSync(
        `${directory}/${String(seq).padStart(8, "0")}.json`,
        encode({ ...body, seq, key: seq.toString(16).padStart(64, "0") }),
        { mode: 0o600 },
      );
    }
    const inbox = await start(directory);
    assert.equal(inbox.available, true);
    await inbox.observe(event("GROUP_ADD_ROBOT", "last"));
    await assert.rejects(inbox.observe(event("FRIEND_DEL", "overflow")), failure("storage"));
    assert.equal(inbox.available, false);
    await inbox.observe(delivery);
    const page = await inbox.page({ afterSeq: 8191 });
    assert.deepEqual(page.items.map(({ seq, type }) => [seq, type]), [[8192, "GROUP_ADD_ROBOT"]]);
    assert.equal(page.throughSeq, 8192);
    assert.equal(page.hasMore, false);
    assert.equal(names(directory).length, 8192);
  }));
