import { lookup } from "node:dns/promises";
import { request as httpRequest } from "node:http";
import { request as httpsRequest } from "node:https";
import { isIP } from "node:net";
import type { WebApi, WebFetchResult, WebSearchResult } from "../contracts/web.ts";

const MAX_BYTES = 2 * 1024 * 1024;
const MAX_REDIRECTS = 3;
export interface WebAddress {
  address: string;
  family: number;
}
export interface WebResponse {
  status: number;
  headers: Record<string, string>;
  body: Uint8Array;
}
export interface WebOptions {
  resolve?: (hostname: string) => Promise<WebAddress[]>;
  transport?: (url: URL, address: WebAddress, signal: AbortSignal) => Promise<WebResponse>;
  timeoutMs?: number;
}

/** Fail closed for local, special-purpose, mapped and transition address ranges. */
function publicAddress(address: string): boolean {
  if (isIP(address) === 4) {
    const [a, b, c] = address.split(".").map(Number);
    return !(a === 0 || a === 10 || a === 127 || a >= 224 ||
      (a === 100 && b >= 64 && b <= 127) ||
      (a === 169 && b === 254) || (a === 172 && b >= 16 && b <= 31) ||
      (a === 192 && (b === 0 || b === 168 || (b === 88 && c === 99))) ||
      (a === 198 && (b === 18 || b === 19 || (b === 51 && c === 100))) ||
      (a === 203 && b === 0 && c === 113));
  }
  if (isIP(address) !== 6) return false;
  const pieces = address.toLowerCase().split(":");
  const first = parseInt(pieces[0], 16), second = parseInt(pieces[1] || "0", 16);
  return first >= 0x2000 && first <= 0x3fff && first !== 0x2002 &&
    !(first === 0x2001 && (second <= 0x1ff || second === 0xdb8));
}

function safeUrl(raw: string): URL {
  const url = new URL(raw);
  if (
    !["http:", "https:"].includes(url.protocol) || url.username || url.password ||
    (url.port && url.port !== "80" && url.port !== "443")
  ) {
    throw new Error("Web URL requires public HTTP(S), port 80/443, and no credentials");
  }
  url.hash = "";
  return url;
}

function transport(url: URL, address: WebAddress, signal: AbortSignal): Promise<WebResponse> {
  return new Promise((resolve, reject) => {
    // No proxy agents or environment proxy settings: connect directly to the pinned IP.
    // The URL hostname remains the HTTP Host and TLS verification/SNI identity.
    const request = (url.protocol === "https:" ? httpsRequest : httpRequest)(url, {
      agent: false,
      signal,
      lookup: (_hostname, options, callback) => {
        if (options.all) callback(null, [address]);
        else callback(null, address.address, address.family);
      },
      headers: {
        "user-agent": "EveWeb/1.0 (public-document reader)",
        accept: "text/html,application/xhtml+xml,text/plain,application/rss+xml,application/xml",
        "accept-encoding": "identity",
      },
    }, (response) => {
      const chunks: Uint8Array[] = [];
      let size = 0;
      response.on("error", reject);
      response.on("data", (chunk: Uint8Array) => {
        size += chunk.byteLength;
        if (size > MAX_BYTES) {
          response.destroy(new Error("Web response exceeds 2 MiB"));
          return;
        }
        chunks.push(chunk);
      });
      response.on("end", () => {
        const body = new Uint8Array(size);
        let offset = 0;
        for (const chunk of chunks) {
          body.set(chunk, offset);
          offset += chunk.length;
        }
        const headers: Record<string, string> = {};
        for (const [name, value] of Object.entries(response.headers)) {
          if (value !== undefined) headers[name] = Array.isArray(value) ? value.join(", ") : value;
        }
        resolve({ status: response.statusCode ?? 0, headers, body });
      });
    });
    request.on("error", reject);
    request.end();
  });
}

function entities(text: string): string {
  const named: Record<string, string> = {
    amp: "&",
    lt: "<",
    gt: ">",
    quot: '"',
    apos: "'",
    nbsp: " ",
  };
  return text.replace(/&(#x[\da-f]+|#\d+|amp|lt|gt|quot|apos|nbsp);/gi, (raw, key) => {
    if (key[0] !== "#") return named[key.toLowerCase()] ?? raw;
    const n = key[1].toLowerCase() === "x" ? parseInt(key.slice(2), 16) : Number(key.slice(1));
    return n > 0 && n <= 0x10ffff && !(n >= 0xd800 && n <= 0xdfff) ? String.fromCodePoint(n) : "�";
  });
}
function plain(html: string): string {
  const output: string[] = [];
  const suppressed = new Set(["script", "style", "noscript", "svg", "nav", "footer", "header"]);
  let cursor = 0, hidden = "";
  while (cursor < html.length) {
    const open = html.indexOf("<", cursor);
    if (open < 0) {
      if (!hidden) output.push(html.slice(cursor));
      break;
    }
    if (!hidden) output.push(html.slice(cursor, open));
    if (html.startsWith("<!--", open)) {
      const close = html.indexOf("-->", open + 4);
      if (close < 0) break;
      cursor = close + 3;
      continue;
    }
    const close = html.indexOf(">", open + 1);
    if (close < 0) break;
    const tag = html.slice(open + 1, close).trim().toLowerCase();
    const name = tag.match(/^\/?([a-z][a-z0-9]*)/)?.[1] ?? "";
    if (hidden) {
      if (tag.startsWith("/") && name === hidden) hidden = "";
    } else if (suppressed.has(name) && !tag.startsWith("/")) hidden = name;
    else output.push(/^(?:\/(?:p|div|section|article|h[1-6]|li|tr)\b|br\b)/.test(tag) ? "\n" : " ");
    cursor = close + 1;
  }
  return entities(output.join(""))
    .replace(/[ \t\r]+/g, " ").replace(/ *\n */g, "\n").replace(/\n{3,}/g, "\n\n").trim();
}
// Match individual bounded tags once: missing closing tags never cause repeated suffix scans.
function* elements(html: string, name: string) {
  const tags = new RegExp(`<(/?)${name}\\b([^<>]*)>`, "gi");
  let opened: { attributes: string; start: number; content: number } | undefined;
  for (const token of html.matchAll(tags)) {
    if (!token[1]) {
      opened ??= {
        attributes: token[2],
        start: token.index!,
        content: token.index! + token[0].length,
      };
    } else if (opened) {
      yield {
        attributes: opened.attributes,
        inner: html.slice(opened.content, token.index),
        index: opened.start,
        end: token.index! + token[0].length,
      };
      opened = undefined;
    }
  }
}
function field(xml: string, name: string): string {
  const value = elements(xml, name).next().value?.inner ?? "";
  return plain(value.replace("<![CDATA[", "").replace("]]>", ""));
}
function bounded(value: number | undefined, fallback: number, max: number): number {
  if (value === undefined) return fallback;
  if (!Number.isSafeInteger(value) || value < 1 || value > max) {
    throw new Error(`Expected integer between 1 and ${max}`);
  }
  return value;
}

/** Read-only public web access. All returned content is untrusted external data. */
export class WebService implements WebApi {
  #resolve: NonNullable<WebOptions["resolve"]>;
  #transport: NonNullable<WebOptions["transport"]>;
  #timeoutMs: number;
  constructor(options: WebOptions = {}) {
    this.#resolve = options.resolve ??
      ((hostname) => lookup(hostname, { all: true, verbatim: true }));
    this.#transport = options.transport ?? transport;
    this.#timeoutMs = options.timeoutMs ?? 20_000;
  }

  async #read(raw: string): Promise<{ url: string; text: string; type: string }> {
    const signal = AbortSignal.timeout(this.#timeoutMs);
    const operation = async () => {
      let url = safeUrl(raw);
      for (let redirects = 0;; redirects++) {
        const hostname = url.hostname.replace(/^\[|\]$/g, "");
        const addresses = isIP(hostname)
          ? [{ address: hostname, family: isIP(hostname) }]
          : await this.#resolve(hostname);
        if (!addresses.length || addresses.some((a) => !publicAddress(a.address))) {
          throw new Error("Web destination resolves to a non-public address");
        }
        signal.throwIfAborted();
        const response = await this.#transport(
          url,
          addresses.find((a) => a.family === 4) ?? addresses[0],
          signal,
        );
        signal.throwIfAborted();
        if (response.body.byteLength > MAX_BYTES) throw new Error("Web response exceeds 2 MiB");
        if ([301, 302, 303, 307, 308].includes(response.status)) {
          if (redirects >= MAX_REDIRECTS || !response.headers.location) {
            throw new Error("Web redirect limit or missing Location");
          }
          url = safeUrl(new URL(response.headers.location, url).href);
          continue;
        }
        if (response.status < 200 || response.status >= 300) {
          throw new Error(`Web HTTP ${response.status}`);
        }
        if (
          response.headers["content-encoding"] &&
          response.headers["content-encoding"] !== "identity"
        ) {
          throw new Error("Compressed Web responses are unsupported");
        }
        const type = response.headers["content-type"] ?? "";
        if (!/^(text\/|application\/(xhtml\+xml|rss\+xml|xml|json)\b)/i.test(type)) {
          throw new Error("Web response is not a supported text document");
        }
        const charset = type.match(/charset=["']?([^;\s"']+)/i)?.[1] ?? "utf-8";
        return { url: url.href, text: new TextDecoder(charset).decode(response.body), type };
      }
    };
    return await new Promise((resolve, reject) => {
      const abort = () => reject(new Error("Web request timed out"));
      signal.addEventListener("abort", abort, { once: true });
      operation().then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
    });
  }

  async fetch(args: {
    url: string;
    startChar?: number;
    limit?: number;
    expectedSha256?: string;
  }): Promise<WebFetchResult> {
    if (typeof args.url !== "string" || args.url.length > 8192) throw new Error("Invalid Web URL");
    const limit = bounded(args.limit, 4096, 8192), start = args.startChar ?? 0;
    if (!Number.isSafeInteger(start) || start < 0) throw new Error("Invalid startChar");
    const document = await this.#read(args.url);
    const html = /html/i.test(document.type);
    const title = html ? field(document.text, "title") : "";
    const content = html ? plain(document.text) : document.text;
    const sha256 = Array.from(
      new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(content))),
      (byte) => byte.toString(16).padStart(2, "0"),
    ).join("");
    if (args.expectedSha256 !== undefined && args.expectedSha256 !== sha256) {
      throw new Error("Web document changed; restart pagination with the new sha256");
    }
    if (start > content.length) throw new Error("startChar exceeds document length");
    const end = Math.min(content.length, start + limit);
    return {
      sourceUrl: document.url,
      title,
      fetchedAt: new Date().toISOString(),
      untrusted: true,
      content: content.slice(start, end),
      startChar: start,
      sha256,
      characters: content.length,
      ...(end < content.length ? { nextStartChar: end } : {}),
      truncated: end < content.length,
    };
  }

  async search(args: { query: string; limit?: number }): Promise<WebSearchResult> {
    if (typeof args.query !== "string" || !args.query.trim() || args.query.length > 1000) {
      throw new Error("Search query must contain 1–1000 characters");
    }
    const limit = bounded(args.limit, 5, 10);
    const results: { title: string; url: string; snippet: string }[] = [];
    let sourceUrl = "";
    const stopwords = new Set([
      "a",
      "an",
      "the",
      "how",
      "what",
      "where",
      "is",
      "are",
      "to",
      "of",
      "in",
      "for",
      "and",
      "api",
      "docs",
      "documentation",
      "official",
      "site",
      "com",
      "org",
      "www",
      "https",
      "http",
    ]);
    const terms = (args.query.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? [])
      .filter((term) => term.length > 1 && !stopwords.has(term));
    const add = (title: string, raw: string, snippet: string) => {
      try {
        if (results.length >= limit || raw.length > 8192) return;
        title = title.slice(0, 500);
        snippet = snippet.slice(0, 1000);
        const url = safeUrl(raw).href;
        const corpus = `${title} ${snippet} ${decodeURI(url)}`.toLowerCase();
        if (terms.length && !terms.some((term) => corpus.includes(term))) return;
        if (!results.some((r) => r.url === url)) results.push({ title, url, snippet });
      } catch { /* Ignore unsupported search links. Fetch still checks DNS before access. */ }
    };
    try {
      const document = await this.#read(
        `https://search.yahoo.com/search?p=${encodeURIComponent(args.query)}`,
      );
      sourceUrl = document.url;
      for (const match of elements(document.text, "a")) {
        const heading = field(match.inner, "h3");
        const raw = match.attributes.match(/href=["']([^"']+)["']/i)?.[1];
        if (!heading || !raw) continue;
        let link = new URL(entities(raw), document.url);
        const redirect = link.pathname.match(/\/RU=([^/]+)/)?.[1];
        if (link.hostname.endsWith("yahoo.com") && redirect) {
          link = new URL(decodeURIComponent(redirect));
        }
        const tail = document.text.slice(match.end, match.end + 4000).split(/<li\b/i)[0];
        const snippet = field(tail, "p");
        add(plain(heading), link.href, plain(snippet).slice(0, 1000));
      }
    } catch { /* Fall back when the public HTML source is unavailable. */ }
    if (!results.length) {
      try {
        const document = await this.#read(
          `https://www.bing.com/search?format=rss&q=${encodeURIComponent(args.query)}`,
        );
        sourceUrl = document.url;
        for (const match of elements(document.text, "item")) {
          add(
            field(match.inner, "title"),
            field(match.inner, "link"),
            field(match.inner, "description"),
          );
        }
      } catch { /* Public fallback if RSS is unavailable. */ }
    }
    if (!results.length) {
      const document = await this.#read(
        `https://html.duckduckgo.com/html/?q=${encodeURIComponent(args.query)}`,
      );
      sourceUrl = document.url;
      for (const match of elements(document.text, "a")) {
        if (!/\bclass=["'][^"']*result__a[^"']*["']/i.test(match.attributes)) continue;
        const raw = match.attributes.match(/href=["']([^"']+)["']/i)?.[1];
        if (!raw) continue;
        const link = new URL(entities(raw), document.url);
        add(plain(match.inner), link.searchParams.get("uddg") ?? link.href, "");
      }
      if (!results.length) throw new Error("Public search returned no usable results");
    }
    return {
      query: args.query,
      title: sourceUrl.includes("yahoo.com")
        ? "Yahoo search"
        : sourceUrl.includes("bing.com")
        ? "Bing search"
        : "DuckDuckGo search",
      sourceUrl,
      fetchedAt: new Date().toISOString(),
      untrusted: true,
      results: results.slice(0, limit),
    };
  }
}
