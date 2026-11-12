import { buildDeploymentPlan, type DeploymentOptions, stageDeployment } from "./plan.ts";

/** Input file contains paths/UIDs/source manifest, never inline model or QQ secrets. */
export async function prepareDeployment(optionsFile: string, apply: boolean) {
  const options = JSON.parse(await Deno.readTextFile(optionsFile)) as DeploymentOptions & {
    existingConfigFile: string;
    unitDirectory: string;
  };
  const existing = JSON.parse(await Deno.readTextFile(options.existingConfigFile));
  const plan = buildDeploymentPlan(existing, options);
  if (apply) await stageDeployment(plan, options.source, options.unitDirectory);
  // Paths to secret files and their contents are deliberately excluded from terminal output.
  console.log(JSON.stringify({ mode: apply ? "staged" : "dry-run", ...plan.summary }));
}
if (import.meta.main) {
  if (Deno.args.length !== 2 || !["--dry-run", "--stage"].includes(Deno.args[0])) {
    throw new Error("参数：--dry-run|--stage 部署选项JSON绝对路径；本入口不运行systemctl");
  }
  await prepareDeployment(Deno.args[1], Deno.args[0] === "--stage");
}
