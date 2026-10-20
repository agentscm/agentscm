import assert from "node:assert/strict";
import http from "node:http";
import net from "node:net";
import test from "node:test";
import {
  createIngressServer,
  createRateLimiter,
  createTestHandler,
  startFromEnvironment,
  toPostHogBatch,
  validateBatch,
} from "./server.mjs";

function event(overrides = {}) {
  return {
    event_id: "0123456789abcdef0123456789abcdef",
    day: "2026-10-20",
    version: "0.1.0",
    os: "linux",
    arch: "x86_64",
    feature: "quickstart",
    outcome: "succeeded",
    count: 1,
    ...overrides,
  };
}

function batch(overrides = {}) {
  return {
    schema: "agentscm.telemetry.batch/v1",
    source: { kind: "installation", id: "abcdef0123456789abcdef0123456789" },
    events: [event()],
    ...overrides,
  };
}

async function listen(handler) {
  const server = http.createServer(handler);
  return listenServer(server);
}

async function listenServer(server) {
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, url: `http://127.0.0.1:${server.address().port}` };
}

async function close(server) {
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
}

async function post(url, value, path = "/v1/telemetry/batches") {
  return fetch(`${url}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: typeof value === "string" ? value : JSON.stringify(value),
  });
}

test("validates the exact public schema and rejects unknown or duplicate fields", () => {
  assert.equal(validateBatch(batch()), null);
  assert.match(validateBatch({ ...batch(), extra: true }), /envelope/);
  assert.match(validateBatch(batch({ events: [event(), event()] })), /event id/);
  assert.match(validateBatch(batch({ events: [event({ day: "2026-02-30" })] })), /metadata/);
  assert.match(validateBatch(batch({ events: [event({ count: 2 })] })), /count/);
  assert.equal(validateBatch(batch({ events: [event({ version: "0.1.0-alpha.3+build.7" })] })), null);
  assert.match(validateBatch(batch({ events: [event({ version: { private: "text" } })] })), /metadata/);
});

test("translation is allowlisted, anonymous, location-disabled, and stable across retries", () => {
  const first = toPostHogBatch(batch(), "test-token");
  const second = toPostHogBatch(batch(), "test-token");
  assert.deepEqual(first, second);
  assert.equal(first.batch[0].uuid, "01234567-89ab-cdef-0123-456789abcdef");
  assert.equal(first.batch[0].properties.$insert_id, event().event_id);
  assert.equal(first.batch[0].properties.$process_person_profile, false);
  assert.equal(first.batch[0].properties.$geoip_disable, true);
  assert.equal(first.batch[0].timestamp, "2026-10-20T00:00:00.000Z");
  assert.equal(first.batch[0].properties.distinct_id, "installation:abcdef0123456789abcdef0123456789");
  assert.deepEqual(Object.keys(first.batch[0].properties).sort(), [
    "$geoip_disable", "$insert_id", "$process_person_profile", "arch", "count", "day",
    "distinct_id", "feature", "os", "outcome", "schema", "source_kind", "version",
  ]);
});

test("forwards an accepted batch without forwarding request metadata", async () => {
  let received;
  const upstream = await listen(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    received = { headers: req.headers, body: JSON.parse(Buffer.concat(chunks).toString("utf8")) };
    res.writeHead(200, { "content-type": "application/json" });
    res.end('{"status":"Ok"}');
  });
  const ingress = await listen(createTestHandler({ projectToken: "test-token", backendUrl: new URL(`${upstream.url}/batch/`) }));
  try {
    const response = await post(ingress.url, batch());
    assert.equal(response.status, 202);
    assert.deepEqual(await response.json(), { status: "accepted" });
    assert.equal(received.headers.authorization, undefined);
    assert.equal(received.headers.cookie, undefined);
    assert.equal(received.headers["user-agent"], undefined);
    assert.equal(received.headers.forwarded, undefined);
    assert.equal(received.body.token, "test-token");
  } finally {
    await close(ingress.server);
    await close(upstream.server);
  }
});

test("keeps backend overrides inside the loopback-only test boundary", () => {
  assert.throws(() => createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL("https://example.test/batch/"),
  }), /loopback HTTP/);
});

test("reports actual backend timeout and rate limit failures", async () => {
  const slowUpstream = await listen((_req, res) => setTimeout(() => res.end('{"status":"Ok"}'), 100));
  const timeoutIngress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${slowUpstream.url}/batch/`),
    timeoutMs: 20,
  }));
  try {
    const response = await post(timeoutIngress.url, batch());
    assert.equal(response.status, 503);
  } finally {
    await close(timeoutIngress.server);
    await close(slowUpstream.server);
  }

  const trickleUpstream = await listen((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.write('{"status":"Ok"');
    const interval = setInterval(() => res.write(" "), 5);
    setTimeout(() => {
      clearInterval(interval);
      res.end("}");
    }, 100);
  });
  const absoluteTimeoutIngress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${trickleUpstream.url}/batch/`),
    timeoutMs: 20,
  }));
  try {
    assert.equal((await post(absoluteTimeoutIngress.url, batch())).status, 503);
  } finally {
    await close(absoluteTimeoutIngress.server);
    await close(trickleUpstream.server);
  }

  const throttledUpstream = await listen((_req, res) => {
    res.writeHead(429, { "retry-after": "17" });
    res.end('{"error":"throttled"}');
  });
  const throttledIngress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${throttledUpstream.url}/batch/`),
  }));
  try {
    const response = await post(throttledIngress.url, batch());
    assert.equal(response.status, 503);
    assert.equal(response.headers.get("retry-after"), "17");
  } finally {
    await close(throttledIngress.server);
    await close(throttledUpstream.server);
  }

  const acceptingUpstream = await listen((_req, res) => res.end('{"status":"Ok"}'));
  let current = 0;
  const limiter = createRateLimiter({
    globalPerMinute: 3,
    sourcePerMinute: 1,
    now: () => current,
  });
  const limitedIngress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${acceptingUpstream.url}/batch/`),
    limiter,
  }));
  try {
    assert.equal((await post(limitedIngress.url, batch())).status, 202);
    assert.equal((await post(limitedIngress.url, batch())).status, 429);
    assert.equal((await post(limitedIngress.url, batch({
      source: { kind: "installation", id: "1234567890abcdef1234567890abcdef" },
    }))).status, 202);
    assert.equal((await post(limitedIngress.url, batch({
      source: { kind: "installation", id: "fedcba0987654321fedcba0987654321" },
    }))).status, 429);
    current = 60_000;
    assert.equal((await post(limitedIngress.url, batch())).status, 202);
  } finally {
    await close(limitedIngress.server);
    await close(acceptingUpstream.server);
  }
});

test("bounds oversized, slow, and concurrent requests", async () => {
  const upstream = await listen((_req, res) => res.end('{"status":"Ok"}'));
  const ingress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${upstream.url}/batch/`),
    requestBodyTimeoutMs: 20,
  }));
  try {
    const oversized = await post(ingress.url, "x".repeat(64 * 1024 + 1));
    assert.equal(oversized.status, 413);

    const slowStatus = await new Promise((resolve, reject) => {
      const url = new URL(`${ingress.url}/v1/telemetry/batches`);
      const request = http.request(url, {
        method: "POST",
        headers: { "content-type": "application/json", "content-length": "100" },
      }, (response) => {
        response.resume();
        response.on("end", () => resolve(response.statusCode));
      });
      request.on("error", reject);
      request.write("{");
    });
    assert.equal(slowStatus, 408);
  } finally {
    await close(ingress.server);
    await close(upstream.server);
  }

  let release;
  let started;
  const startedPromise = new Promise((resolve) => { started = resolve; });
  const blockingUpstream = await listen((_req, res) => {
    started();
    release = () => res.end('{"status":"Ok"}');
  });
  const boundedIngress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${blockingUpstream.url}/batch/`),
    maxConcurrent: 1,
  }));
  try {
    const first = post(boundedIngress.url, batch());
    await startedPromise;
    assert.equal((await post(boundedIngress.url, batch())).status, 503);
    release();
    assert.equal((await first).status, 202);
  } finally {
    await close(boundedIngress.server);
    await close(blockingUpstream.server);
  }
});

test("does not acknowledge malformed, quota-limited, or redirected requests", async () => {
  const upstream = await listen((_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end('{"status":"Ok","quota_limited":["events"]}');
  });
  const ingress = await listen(createTestHandler({ projectToken: "test-token", backendUrl: new URL(`${upstream.url}/batch/`) }));
  try {
    assert.equal((await post(ingress.url, "{" )).status, 400);
    assert.equal((await post(ingress.url, batch(), "/v1/telemetry/batches?x=1")).status, 404);
    assert.equal((await post(ingress.url, batch())).status, 429);
  } finally {
    await close(ingress.server);
    await close(upstream.server);
  }
});

test("accepts only PostHog's documented HTTP 200 response", async () => {
  let status = 201;
  const upstream = await listen((_req, res) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end('{"status":"Ok"}');
  });
  const ingress = await listen(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${upstream.url}/batch/`),
  }));
  try {
    assert.equal((await post(ingress.url, batch())).status, 502);
    status = 202;
    assert.equal((await post(ingress.url, batch())).status, 502);
  } finally {
    await close(ingress.server);
    await close(upstream.server);
  }
});

test("caps held-header connections and closes early rejected uploads", async () => {
  const upstream = await listen((_req, res) => res.end('{"status":"Ok"}'));
  const guardedServer = createIngressServer(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${upstream.url}/batch/`),
  }), { maxConnections: 1, maxHeaderBytes: 1024 });
  const guarded = await listenServer(guardedServer);
  let held;
  try {
    held = net.createConnection(new URL(guarded.url).port, "127.0.0.1");
    await new Promise((resolve, reject) => {
      held.once("connect", resolve);
      held.once("error", reject);
    });
    held.write("POST /v1/telemetry/batches HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Held:");
    assert.equal(await new Promise((resolve, reject) => {
      guardedServer.getConnections((error, count) => error ? reject(error) : resolve(count));
    }), 1);
    await new Promise((resolve, reject) => {
      const extra = net.createConnection(new URL(guarded.url).port, "127.0.0.1");
      const timer = setTimeout(() => {
        extra.destroy();
        reject(new Error("extra connection was not closed"));
      }, 500);
      extra.once("close", () => {
        clearTimeout(timer);
        resolve();
      });
      extra.once("error", () => {});
    });
  } finally {
    held?.destroy();
    await close(guarded.server);
    await close(upstream.server);
  }

  const rejectUpstream = await listen((_req, res) => res.end('{"status":"Ok"}'));
  const rejecting = await listenServer(createIngressServer(createTestHandler({
    projectToken: "test-token",
    backendUrl: new URL(`${rejectUpstream.url}/batch/`),
  })));
  try {
    const rawResponse = await new Promise((resolve, reject) => {
      const socket = net.createConnection(new URL(rejecting.url).port, "127.0.0.1");
      let response = "";
      const timer = setTimeout(() => {
        socket.destroy();
        reject(new Error("rejected upload connection remained open"));
      }, 500);
      socket.on("data", (chunk) => { response += chunk.toString("utf8"); });
      socket.once("connect", () => socket.write(
        "POST /v1/telemetry/batches HTTP/1.1\r\n"
        + "Host: 127.0.0.1\r\nContent-Type: text/plain\r\nContent-Length: 60000\r\n\r\n"
        + "x".repeat(1024),
      ));
      socket.once("close", () => {
        clearTimeout(timer);
        resolve(response);
      });
      socket.once("error", reject);
    });
    assert.match(rawResponse, /HTTP\/1\.1 415/);
    assert.match(rawResponse.toLowerCase(), /connection: close/);
  } finally {
    await close(rejecting.server);
    await close(rejectUpstream.server);
  }
});

test("requires an explicit enable switch and backend token", () => {
  assert.throws(() => startFromEnvironment({}), /disabled/);
  assert.throws(() => startFromEnvironment({ AGENTSCM_TELEMETRY_INGRESS_ENABLED: "1" }), /project token/);
});
