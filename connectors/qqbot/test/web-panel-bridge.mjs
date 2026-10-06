import fs from "node:fs";
const scenario = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (["EVE_WEB_TOKEN", "EVE_OPENAI_API_KEY", "EVE_JEV_API_KEY"].some(name => name in process.env)) {
  if (scenario.error_file) fs.writeFileSync(scenario.error_file, "host_credential_leaked");
  throw new Error("host_credential_leaked");
}
await import("./fake-bridge.mjs");
