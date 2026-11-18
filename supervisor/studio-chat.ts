import { DatabaseSync } from "node:sqlite";
import { dirname, resolve } from "node:path";
import type { Message } from "../contracts/model.ts";
import { KernelError } from "../kernel/protocol.ts";
import {
  bytes,
  CHAT_DESCRIPTION,
  CHAT_MODE,
  CHAT_PAGE,
  CHAT_TURNS,
  chatError,
  type ChatMessage,
  type ChatPage,
  chatRequest,
  type ChatStatus,
} from "../services/admin/studio-chat-contract.ts";

type Turn = {
  turn: number;
  id: string;
  text: string;
  at: number;
  state: ChatStatus;
  slot: string | null;
  reply: string | null;
  replied: number | null;
};

function userMessage(turn: Turn): ChatMessage {
  return {
    seq: turn.turn * 2 - 1,
    id: turn.id,
    role: "user",
    text: turn.text,
    status: turn.state,
    at: new Date(turn.at).toISOString(),
  };
}

function storage<T>(operation: () => T): T {
  try {
    return operation();
  } catch (error) {
    if (error instanceof KernelError) throw error;
    throw chatError("unavailable");
  }
}

function checkPrivate(path: string, directory: boolean, owner: number | null) {
  const stat = Deno.lstatSync(path);
  if (
    (directory ? !stat.isDirectory : !stat.isFile || stat.nlink !== 1) || stat.isSymlink ||
    owner === null || stat.uid !== owner || ((stat.mode ?? 0o777) & 0o077) !== 0 ||
    Deno.realPathSync(path) !== path
  ) throw chatError("unavailable");
}

const SCHEMA = `
CREATE TABLE turns (
  turn INTEGER PRIMARY KEY CHECK(turn BETWEEN 1 AND 512),
  id TEXT NOT NULL UNIQUE CHECK(length(id)=36),
  text TEXT NOT NULL CHECK(length(CAST(text AS BLOB)) BETWEEN 1 AND 12288),
  at INTEGER NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('queued','processing','answered','failed','unknown')),
  slot TEXT UNIQUE,
  reply TEXT, replied INTEGER,
  CHECK ((slot IS NULL) = (state='queued')),
  CHECK ((reply IS NOT NULL) = (state='answered') AND (replied IS NOT NULL) = (state='answered')),
  CHECK (length(CAST(reply AS BLOB)) BETWEEN 1 AND 8192)
) STRICT;
CREATE UNIQUE INDEX one_outstanding ON turns((1)) WHERE state IN ('queued','processing');
PRAGMA application_id=1163285315; PRAGMA user_version=1;`;

export class StudioChatMailbox {
  private constructor(private readonly db: DatabaseSync) {}

  static open(directory: string): StudioChatMailbox {
    return storage(() => {
      if (resolve(directory) !== directory || directory === "/") throw chatError("unavailable");
      const parent = dirname(directory), trusted = Deno.lstatSync(parent);
      if (
        !trusted.isDirectory || Deno.realPathSync(parent) !== parent ||
        ((trusted.mode ?? 0o777) & 0o022)
      ) {
        throw chatError("unavailable");
      }
      try {
        Deno.mkdirSync(directory, { mode: 0o700 });
      } catch (error) {
        if (!(error instanceof Deno.errors.AlreadyExists)) throw error;
      }
      checkPrivate(directory, true, trusted.uid);
      const path = `${directory}/chat.sqlite`;
      try {
        Deno.openSync(path, { createNew: true, write: true, mode: 0o600 }).close();
      } catch (error) {
        if (!(error instanceof Deno.errors.AlreadyExists)) throw error;
      }
      checkPrivate(path, false, trusted.uid);
      const db = new DatabaseSync(path, { allowExtension: false });
      try {
        db.exec(`PRAGMA trusted_schema=OFF; PRAGMA busy_timeout=250; PRAGMA synchronous=FULL;
          PRAGMA journal_mode=DELETE; PRAGMA max_page_count=4096; BEGIN IMMEDIATE`);
        const version = db.prepare("PRAGMA user_version").get()!.user_version;
        const app = db.prepare("PRAGMA application_id").get()!.application_id;
        if (
          version === 0 && app === 0 && !db.prepare("SELECT 1 FROM sqlite_schema LIMIT 1").get()
        ) {
          db.exec(SCHEMA);
        } else if (version !== 1 || app !== 1163285315) throw chatError("unavailable");
        db.exec("COMMIT");
        return new StudioChatMailbox(db);
      } catch (error) {
        db.close();
        throw error;
      }
    });
  }

  #write<T>(change: () => T): T {
    return storage(() => {
      this.db.exec("BEGIN IMMEDIATE");
      try {
        const result = change();
        this.db.exec("COMMIT");
        return result;
      } catch (error) {
        this.db.exec("ROLLBACK");
        throw error;
      }
    });
  }

  handle(input: unknown) {
    const request = chatRequest(input);
    if (request.action === "chat") return this.page(request.before);
    return this.#write(() => {
      const old = this.db.prepare("SELECT * FROM turns WHERE id=?").get(request.id) as
        | Turn
        | undefined;
      if (old) {
        if (old.text !== request.text) throw chatError("conflict");
        return { message: userMessage(old), mode: CHAT_MODE };
      }
      if (this.db.prepare("SELECT 1 FROM turns WHERE state IN ('queued','processing')").get()) {
        throw chatError("busy");
      }
      if (Number(this.db.prepare("SELECT count(*) AS n FROM turns").get()!.n) >= CHAT_TURNS) {
        throw chatError("full");
      }
      const turn = this.db.prepare(
        "INSERT INTO turns(id,text,at,state) VALUES(?,?,?,'queued') RETURNING *",
      ).get(request.id, request.text, Date.now()) as Turn;
      return { message: userMessage(turn), mode: CHAT_MODE };
    });
  }

  page(before = Number.MAX_SAFE_INTEGER): ChatPage {
    if (!Number.isSafeInteger(before) || before < 1) throw chatError("invalid");
    return storage(() => {
      const turns = this.db.prepare("SELECT * FROM turns WHERE turn<=? ORDER BY turn DESC LIMIT ?")
        .all(Math.floor(before / 2), CHAT_PAGE + 1) as Turn[];
      const descending: ChatMessage[] = [];
      for (const turn of turns) {
        if (turn.state === "answered" && turn.turn * 2 < before) {
          descending.push({
            seq: turn.turn * 2,
            id: `reply:${turn.id}`,
            replyTo: turn.id,
            role: "assistant",
            text: turn.reply!,
            status: "sent",
            at: new Date(turn.replied!).toISOString(),
          });
        }
        descending.push(userMessage(turn));
        if (descending.length > CHAT_PAGE) break;
      }
      const messages = descending.slice(0, CHAT_PAGE).reverse();
      return {
        messages,
        nextBefore: descending.length > CHAT_PAGE ? messages[0].seq : null,
        mode: CHAT_MODE,
        description: CHAT_DESCRIPTION,
      };
    });
  }

  claim(slot: string): { used: boolean; id?: string; history?: Message[] } {
    if (!/^studio_slot_[1-9][0-9]{0,15}$/.test(slot)) throw chatError("invalid");
    return this.#write(() => {
      if (this.db.prepare("SELECT 1 FROM turns WHERE slot=?").get(slot)) return { used: true };
      const turn = this.db.prepare(
        "UPDATE turns SET state='processing',slot=? WHERE state='queued' RETURNING *",
      ).get(slot) as Turn | undefined;
      if (!turn) return { used: false };
      const history: Message[] = [];
      let size = bytes(turn.text);
      const recent = this.db.prepare(
        "SELECT text,reply FROM turns WHERE turn<? AND state='answered' ORDER BY turn DESC LIMIT 6",
      ).all(turn.turn);
      for (const previous of recent) {
        const text = String(previous.text), reply = String(previous.reply);
        size += bytes(text) + bytes(reply);
        if (size > 24_576) break;
        history.unshift({ role: "user", content: text }, { role: "assistant", content: reply });
      }
      history.push({ role: "user", content: turn.text });
      return { used: true, id: turn.id, history };
    });
  }

  inFlight(): string | undefined {
    return storage(() =>
      this.db.prepare("SELECT id FROM turns WHERE state='processing'").get()?.id as
        | string
        | undefined
    );
  }

  queued(): boolean {
    return storage(() => !!this.db.prepare("SELECT 1 FROM turns WHERE state='queued'").get());
  }

  queuedTurn(): number | null {
    return storage(() => {
      const turn = this.db.prepare("SELECT turn FROM turns WHERE state='queued'").get()?.turn;
      return turn === undefined ? null : Number(turn);
    });
  }

  finish(id: string, state: "answered" | "unknown" | "failed", text?: string) {
    this.#write(() => {
      const updated = this.db.prepare(
        "UPDATE turns SET state=?, reply=?, replied=? WHERE id=? AND state='processing'",
      ).run(state, text ?? null, state === "answered" ? Date.now() : null, id);
      if (updated.changes !== 1) throw chatError("unavailable");
    });
  }

  close() {
    this.db.close();
  }
}
