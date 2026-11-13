/** One lazy local WASM encoder; no semantic service or chat model call. */
let encoder: Promise<import("tiktoken").Tiktoken> | undefined;
export async function countSelfTokens(text: string) {
  encoder ??= import("tiktoken").then(({ get_encoding }) => get_encoding("o200k_base"));
  return { tokens: (await encoder).encode(text, [], []).length, encoding: "o200k_base" };
}
