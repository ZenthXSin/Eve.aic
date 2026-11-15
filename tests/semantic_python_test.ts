import assert from "node:assert/strict";
Deno.test("semantic index: chunk tails, restart, revisions, original deletion and source boundaries", async () => {
  const result = await new Deno.Command("python3", {
    args: ["-B", new URL("./semantic_index_test.py", import.meta.url).pathname],
    stdout: "piped",
    stderr: "piped",
  }).output();
  assert.equal(result.success, true, new TextDecoder().decode(result.stderr));
});
