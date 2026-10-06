// Test-only wrapper: reuse the offline JSONL bridge after checking host isolation.
// Never include the credential value in an error or event.
import fs from "node:fs";

const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
const leaked = ["EVE_OPENAI_API_KEY", "EVE_JEV_API_KEY"].some(name => name in process.env)
  || process.argv.some(value => /test-(?:app|model|jev)-secret/.test(value));
if (leaked) {
  if (scenario.error_file) fs.writeFileSync(scenario.error_file, "judge_credentials_leaked");
  throw new Error("judge_credentials_leaked");
}
await import("./fake-bridge.mjs");
