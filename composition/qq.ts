import { QqClient, QqRequestError } from "../adapters/qq/client.ts";
import { RestrictedFileTools } from "../services/runtime/restricted-files.ts";

export async function readQqTestClient(configPath: string): Promise<QqClient> {
  try {
    const config = JSON.parse(await Deno.readTextFile(configPath));
    const qq = config.qq;
    if (
      !qq || qq.sandbox !== false ||
      (qq.replyScope !== undefined && !["private", "all"].includes(qq.replyScope)) ||
      typeof qq.appIdFile !== "string" || !qq.appIdFile.startsWith("/") ||
      typeof qq.appSecretFile !== "string" || !qq.appSecretFile.startsWith("/")
    ) throw new Error();
    const appId = (await Deno.readTextFile(qq.appIdFile)).trim();
    const appSecret = (await Deno.readTextFile(qq.appSecretFile)).trim();
    let attachmentReader: ((path: string) => Promise<Uint8Array>) | undefined;
    const sources = [
      ...(qq.mediaRoots !== undefined || qq.mediaIdentity !== undefined ||
          qq.mediaRuntime !== undefined
        ? [{ roots: qq.mediaRoots, identity: qq.mediaIdentity, runtime: qq.mediaRuntime }]
        : []),
    ];
    if (qq.mediaSources !== undefined) {
      if (!Array.isArray(qq.mediaSources) || qq.mediaSources.length > 8) throw new Error();
      sources.push(...qq.mediaSources);
    }
    const readers: { roots: string[]; files: RestrictedFileTools }[] = [];
    for (const source of sources) {
      if (
        !source || !Array.isArray(source.roots) || source.roots.length < 1 ||
        source.roots.some((path: unknown) => typeof path !== "string" || !path.startsWith("/")) ||
        !source.identity
      ) throw new Error();
      const files = new RestrictedFileTools(
        source.roots[0],
        source.roots,
        source.identity,
        source.runtime,
      );
      readers.push({
        roots: source.roots.map((root: string) => root.replace(/\/+$/, "") || "/"),
        files,
      });
    }
    if (readers.length) {
      attachmentReader = (path) => {
        // Pick the most specific configured root. The child still checks the real
        // path using that source's Unix identity; never fall back to root reads.
        const matches = readers.flatMap(({ roots, files }) =>
          roots.filter((root) => root === "/" || path === root || path.startsWith(root + "/"))
            .map((root) => ({ root, files }))
        ).sort((a, b) => b.root.length - a.root.length);
        if (!matches.length) throw new QqRequestError({ category: "request" });
        return matches[0].files.readBytes({ path, maxBytes: 300 * 1024 * 1024 });
      };
    }
    return new QqClient({ appId, appSecret }, {
      sendScope: qq.replyScope ?? "private",
      attachmentReader,
      diagnostics: (event) => console.log(JSON.stringify({ qqTransport: event })),
    });
  } catch {
    throw new QqRequestError({ category: "request" });
  }
}
