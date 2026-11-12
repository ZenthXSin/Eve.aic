import assert from "node:assert/strict";
import { type WebResponse, WebService } from "../services/web.ts";

function response(text: string, type = "text/html"): WebResponse {
  return { status: 200, headers: { "content-type": type }, body: new TextEncoder().encode(text) };
}
const publicDns = () => Promise.resolve([{ address: "93.184.216.34", family: 4 }]);

Deno.test("Web fetch strips active markup, paginates and detects changed documents", async () => {
  let html = "<title>A &amp; B</title><script>evil()</script><nav>menu</nav><p>Hello world</p>";
  const service = new WebService({
    resolve: publicDns,
    transport: () => Promise.resolve(response(html)),
  });
  const first = await service.fetch({ url: "https://example.com/a", limit: 5 }) as {
    content: string;
    nextStartChar: number;
    sha256: string;
    untrusted: boolean;
    title: string;
  };
  assert.equal(first.untrusted, true);
  assert.equal(first.title, "A & B");
  assert.equal(first.content, "A & B");
  const next = await service.fetch({
    url: "https://example.com/a",
    startChar: first.nextStartChar,
    expectedSha256: first.sha256,
  }) as { content: string; nextStartChar?: number };
  assert.match(next.content, /Hello world/);
  assert.doesNotMatch(next.content, /evil|menu/);
  assert.equal(next.nextStartChar, undefined);
  html += "<p>changed</p>";
  await assert.rejects(() =>
    service.fetch({
      url: "https://example.com/a",
      startChar: 5,
      expectedSha256: first.sha256,
    }), /changed/);
});

Deno.test("Web rejects unsafe URL syntax and literal private addresses before transport", async () => {
  let calls = 0;
  const service = new WebService({
    resolve: publicDns,
    transport: () => {
      calls++;
      return Promise.resolve(response("unexpected"));
    },
  });
  for (
    const url of [
      "file:///etc/passwd",
      "ftp://example.com/a",
      "https://user:secret@example.com/",
      "https://example.com:8080/",
      "http://127.1/",
      "http://2130706433/",
      "http://0x7f000001/",
      "http://10.1.2.3/",
      "http://169.254.169.254/latest/meta-data/",
      "http://100.100.100.200/",
      "http://192.168.1.1/",
      "http://198.18.0.1/",
      "http://[::1]/",
      "http://[::ffff:127.0.0.1]/",
      "http://[fc00::1]/",
      "http://[2002:7f00:1::]/",
      "http://[2001:db8::1]/",
    ]
  ) await assert.rejects(() => service.fetch({ url }), Error, url);
  assert.equal(calls, 0);
});

Deno.test("Web rejects mixed public/private DNS and pins a single public lookup", async () => {
  const denied = new WebService({
    resolve: () =>
      Promise.resolve([
        { address: "93.184.216.34", family: 4 },
        { address: "127.0.0.1", family: 4 },
      ]),
    transport: () => {
      throw new Error("must not connect");
    },
  });
  await assert.rejects(() => denied.fetch({ url: "https://example.com" }), /non-public/);
  let lookups = 0;
  const pinned = new WebService({
    resolve: () => {
      lookups++;
      return publicDns();
    },
    transport: (url, address) => {
      assert.equal(url.hostname, "example.com");
      assert.equal(address.address, "93.184.216.34");
      return Promise.resolve(response("ok", "text/plain"));
    },
  });
  await pinned.fetch({ url: "https://example.com" });
  assert.equal(lookups, 1);
});

Deno.test("Web revalidates redirects and bounds redirect chains", async () => {
  let calls = 0;
  const service = new WebService({
    resolve: publicDns,
    transport: () => {
      calls++;
      return Promise.resolve({
        ...response(""),
        status: 302,
        headers: { location: "http://127.0.0.1/" },
      });
    },
  });
  await assert.rejects(() => service.fetch({ url: "https://example.com" }), /non-public/);
  assert.equal(calls, 1);
  calls = 0;
  const looping = new WebService({
    resolve: publicDns,
    transport: () => {
      calls++;
      return Promise.resolve({ ...response(""), status: 302, headers: { location: "/again" } });
    },
  });
  await assert.rejects(() => looping.fetch({ url: "https://example.com" }), /redirect limit/);
  assert.equal(calls, 4);
});

Deno.test("Web bounds bytes, content types, arguments and total timeout including DNS", async () => {
  const large = new WebService({
    resolve: publicDns,
    transport: () =>
      Promise.resolve({
        ...response(""),
        body: new Uint8Array(2 * 1024 * 1024 + 1),
      }),
  });
  await assert.rejects(() => large.fetch({ url: "https://example.com" }), /2 MiB/);
  const binary = new WebService({
    resolve: publicDns,
    transport: () => Promise.resolve(response("x", "image/png")),
  });
  await assert.rejects(() => binary.fetch({ url: "https://example.com" }), /text document/);
  await assert.rejects(() => binary.fetch({ url: "https://example.com", limit: 40001 }));
  await assert.rejects(() => binary.fetch({ url: "https://example.com", startChar: -1 }));
  const stalled = new WebService({ timeoutMs: 10, resolve: () => new Promise(() => {}) });
  await assert.rejects(() => stalled.fetch({ url: "https://example.com" }), /timed out/);
});

Deno.test("Web search parses RSS with bounded unique results", async () => {
  const service = new WebService({
    resolve: publicDns,
    transport: (url) => {
      if (url.hostname === "search.yahoo.com") throw new Error("source unavailable");
      assert.equal(url.searchParams.get("q"), "Eve docs");
      return Promise.resolve(response(
        `<rss><channel><item><title>Eve Docs &amp; guide</title>
      <link>https://example.com/docs</link><description>&lt;b&gt;Public&lt;/b&gt; docs</description>
      </item><item><title>duplicate</title><link>https://example.com/docs</link></item>
      <item><title>bad</title><link>javascript:alert(1)</link></item></channel></rss>`,
        "application/rss+xml",
      ));
    },
  });
  const result = await service.search({ query: "Eve docs" }) as {
    results: { title: string; url: string }[];
    untrusted: boolean;
  };
  assert.equal(result.results.length, 1);
  assert.equal(result.results[0].title, "Eve Docs & guide");
  assert.equal(result.untrusted, true);
  await assert.rejects(() => service.search({ query: " " }));
});

Deno.test("Web search falls back to DuckDuckGo and decodes destination links", async () => {
  let calls = 0;
  const service = new WebService({
    resolve: publicDns,
    transport: (url) => {
      calls++;
      if (url.hostname === "www.bing.com") return Promise.resolve({ ...response(""), status: 503 });
      return Promise.resolve(
        response(
          '<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fguide">Public guide</a>',
        ),
      );
    },
  });
  const result = await service.search({ query: "public guide" }) as { results: { url: string }[] };
  assert.equal(calls, 3);
  assert.equal(result.results[0].url, "https://example.com/guide");
});

Deno.test("Web Yahoo search extracts result headings and rejects unrelated RSS", async () => {
  const service = new WebService({
    resolve: publicDns,
    transport: () =>
      Promise.resolve(response(
        '<a href="https://docs.deno.com/api/"><h3>Deno API</h3></a><p>Deno reference</p>',
      )),
  });
  const result = await service.search({ query: "Deno API" });
  assert.equal(result.title, "Yahoo search");
  assert.deepEqual(result.results, [{
    url: "https://docs.deno.com/api/",
    title: "Deno API",
    snippet: "Deno reference",
  }]);
  const unrelated = new WebService({
    resolve: publicDns,
    transport: () =>
      Promise.resolve(response(
        "<rss><item><title>Lottery</title><link>https://example.com/lottery</link></item></rss>",
      )),
  });
  await assert.rejects(() => unrelated.search({ query: "Deno readTextFile" }), /no usable results/);
});

Deno.test("Web extraction handles large malformed untrusted markup without suffix rescans", async () => {
  let body = "<".repeat(2 * 1024 * 1024);
  const service = new WebService({
    resolve: publicDns,
    transport: () => Promise.resolve(response(body)),
  });
  const first = await service.fetch({ url: "https://example.com/" });
  assert.equal(first.content, "");
  body = "<script>".repeat(100_000);
  const hidden = await service.fetch({ url: "https://example.com/" });
  assert.equal(hidden.content, "");
  body = "<item>".repeat(100_000);
  await assert.rejects(() => service.search({ query: "unmatched" }), /no usable results/);
});
