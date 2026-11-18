const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export function check(condition, message) {
  if (!condition) throw new Error(message);
}

export async function meter(page, origin) {
  const session = await page.context().newCDPSession(page);
  const entries = [], frames = [], pending = new Map(), sockets = new Map();
  let last = performance.now();
  const location = (raw) => {
    const url = new URL(raw);
    const local = url.origin.replace(/^ws/, "http") === origin;
    return local ? url.pathname + url.search : `external:${url.protocol}//${url.host}`;
  };
  session.on("Network.requestWillBeSent", ({ requestId, request, redirectResponse }) => {
    if (!/^https?:/.test(request.url)) return;
    if (redirectResponse && pending.has(requestId)) {
      Object.assign(pending.get(requestId), {
        status: redirectResponse.status,
        bytes: redirectResponse.encodedDataLength,
      });
    }
    const entry = {
      path: location(request.url),
      method: request.method,
      status: null,
      bytes: null,
    };
    entries.push(entry);
    pending.set(requestId, entry);
    last = performance.now();
  });
  session.on("Network.responseReceived", ({ requestId, response }) => {
    const entry = pending.get(requestId);
    if (entry) entry.status = response.status;
  });
  function end(requestId, result) {
    const entry = pending.get(requestId);
    if (entry) {
      Object.assign(entry, result);
      pending.delete(requestId);
      last = performance.now();
    }
  }
  session.on(
    "Network.loadingFinished",
    ({ requestId, encodedDataLength }) => end(requestId, { bytes: encodedDataLength }),
  );
  session.on(
    "Network.loadingFailed",
    ({ requestId, errorText }) => end(requestId, { error: errorText }),
  );
  session.on("Network.webSocketCreated", ({ requestId, url }) => {
    sockets.set(requestId, location(url));
    frames.push({ path: location(url), event: "open" });
  });
  for (
    const [name, event] of [["FrameSent", "sent"], ["FrameReceived", "received"], [
      "Closed",
      "closed",
    ]]
  ) {
    session.on(`Network.webSocket${name}`, ({ requestId }) => {
      frames.push({ path: sockets.get(requestId), event });
      if (event === "closed") sockets.delete(requestId);
    });
  }
  await session.send("Network.enable");
  return {
    entries,
    frames,
    sockets,
    async settle() {
      const deadline = performance.now() + 6000;
      while (pending.size || performance.now() - last < 250) {
        check(
          performance.now() < deadline,
          `network did not settle: ${[...pending.values()].map((entry) => entry.path)}`,
        );
        await sleep(40);
      }
    },
    batch(start) {
      const requests = entries.slice(start);
      return {
        requests,
        bytes: requests.reduce((sum, entry) => sum + (entry.bytes ?? 0), 0),
        assetBytes: requests.filter((entry) =>
          entry.path === "/" || entry.path.startsWith("/studio/")
        )
          .reduce((sum, entry) => sum + (entry.bytes ?? 0), 0),
      };
    },
  };
}
