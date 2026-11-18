import type { ToolSpec } from "../../contracts/model.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import type { ConsciousEvent } from "./journal.ts";
import { lastActiveEvent } from "./view.ts";

export const LANTERNS_TOOL: ToolSpec = {
  name: "play_lanterns",
  description:
    "自己的九宫熄灯小游戏，不联网也不打扰人。每次按一格会切换它及上下左右。1–9 从左上到右下；全灭获胜。随时停，之后在同一盘接着玩；不必为了通关连续调用。",
  parameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      action: { type: "string", enum: ["start", "press", "leave"] },
      cell: { type: "integer", minimum: 1, maximum: 9 },
      expectedSeq: { type: "integer", minimum: 0 },
    },
    required: ["action", "expectedSeq"],
  },
};

function flip(board: number, cell: number) {
  const index = cell - 1;
  return [
    index,
    ...(index >= 3 ? [index - 3] : []),
    ...(index < 6 ? [index + 3] : []),
    ...(index % 3 ? [index - 1] : []),
    ...(index % 3 !== 2 ? [index + 1] : []),
  ].reduce((mask, bit) => mask ^ (1 << bit), board);
}

export function lanternContext(events: ConsciousEvent[]) {
  const last = lastActiveEvent(events, "leisure_lanterns");
  if (!last) {
    return { revision: 0, status: "none" as const, board: 0, moves: 0, rows: [] as string[] };
  }
  const data = last.data as { status: "playing" | "won" | "left"; board: number; moves: number };
  return {
    revision: last.seq,
    status: data.status,
    board: data.board,
    moves: data.moves,
    rows: [0, 3, 6].map((start) =>
      [0, 1, 2].map((offset) => data.board & (1 << (start + offset)) ? "亮" : "暗").join(" ")
    ),
  };
}

export function planLanterns(
  raw: Json,
  events: ConsciousEvent[],
  callId: string,
  seed = () => crypto.getRandomValues(new Uint16Array(1))[0],
) {
  const repeated = lastActiveEvent(
    events,
    "leisure_lanterns",
    (item) => (item.data as { callId?: string }).callId === callId,
  );
  if (repeated) return { existing: repeated.seq } as const;
  const current = lanternContext(events);
  if (
    !raw || typeof raw !== "object" || Array.isArray(raw) || raw.expectedSeq !== current.revision ||
    !["start", "press", "leave"].includes(String(raw.action)) ||
    Object.keys(raw).some((key) => !["action", "expectedSeq", "cell"].includes(key)) ||
    (raw.action === "press"
      ? !Number.isInteger(raw.cell) || Number(raw.cell) < 1 || Number(raw.cell) > 9
      : raw.cell !== undefined)
  ) {
    throw new KernelError("rejected", "棋盘已变化或动作无效，请看当前盘面");
  }
  if (raw.action === "start") {
    if (current.status === "playing") return { existing: current.revision } as const;
    const mask = seed() & 511;
    let board = 0;
    for (let cell = 1; cell <= 9; cell++) if (mask & (1 << (cell - 1))) board = flip(board, cell);
    return { event: { callId, board: board || flip(0, 5), moves: 0, status: "playing" } } as const;
  }
  if (current.status !== "playing") throw new KernelError("rejected", "没有进行中的棋盘");
  const board = raw.action === "press" ? flip(current.board, Number(raw.cell)) : current.board;
  return {
    event: {
      callId,
      board,
      moves: current.moves + (raw.action === "press" ? 1 : 0),
      status: raw.action === "leave" ? "left" : board ? "playing" : "won",
    },
  } as const;
}
