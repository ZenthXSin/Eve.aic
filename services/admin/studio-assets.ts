const files = new Map<string, [string, string]>([
  ["/", ["index.html", "text/html; charset=utf-8"]],
  ...["app", "overview", "chat", "settings", "explore"].map((name): [string, [string, string]] => [
    `/studio/${name}.js`,
    [`${name}.js`, "text/javascript; charset=utf-8"],
  ]),
  ...["base", "overview", "chat", "settings", "explore"].map((name): [string, [string, string]] => [
    `/studio/${name}.css`,
    [`${name}.css`, "text/css; charset=utf-8"],
  ]),
]);

type Asset = { bytes: Uint8Array<ArrayBuffer>; etag: string };
const cached = new Map<string, Promise<Asset>>();

async function load(name: string): Promise<Asset> {
  const bytes = await Deno.readFile(new URL(`./studio/${name}`, import.meta.url));
  const hash = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return { bytes, etag: `"${hash.toHex()}"` };
}

export async function studioAsset(request: Request, path: string): Promise<Response | undefined> {
  if (request.method !== "GET" && request.method !== "HEAD") return undefined;
  const file = files.get(path);
  if (!file) return undefined;
  const [name, contentType] = file;
  if (!cached.has(name)) cached.set(name, load(name));
  const html = path === "/";
  const headers = new Headers({
    "content-type": contentType,
    "cache-control": html ? "no-store" : "private, no-cache",
    "x-content-type-options": "nosniff",
    "referrer-policy": "no-referrer",
    "x-frame-options": "DENY",
    "cross-origin-resource-policy": "same-origin",
    "permissions-policy": "camera=(), microphone=(), geolocation=()",
    "content-security-policy":
      "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'",
  });
  try {
    const { bytes, etag } = await cached.get(name)!;
    headers.set("etag", etag);
    if (!html && request.headers.get("if-none-match") === etag) {
      return new Response(null, { status: 304, headers });
    }
    headers.set("content-length", String(bytes.byteLength));
    return new Response(request.method === "HEAD" ? null : bytes, { headers });
  } catch {
    cached.delete(name);
    headers.set("cache-control", "no-store");
    return new Response(null, { status: 503, headers });
  }
}
