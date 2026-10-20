import http from "node:http";
import https from "node:https";

const MAX_BODY_BYTES = 64 * 1024;
const MAX_EVENTS = 64;
const UPSTREAM_BODY_BYTES = 64 * 1024;
const ID_RE = /^[0-9a-f]{32}$/;
const VERSION_RE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;
const DAY_RE = /^\d{4}-\d{2}-\d{2}$/;
const POSTHOG_URL = new URL("https://eu.i.posthog.com/batch/");

const SOURCE_KINDS = new Set(["installation", "hub"]);
const OS_VALUES = new Set(["linux", "macos", "windows", "other"]);
const ARCH_VALUES = new Set(["x86_64", "aarch64", "other"]);
const FEATURES = new Set([
  "installation", "first_use", "quickstart", "uninstall", "init_repo", "collisions", "autopsy", "intent",
  "work", "native_source", "workflow", "team", "hub", "git_bridge",
  "skill_workflow", "setup",
]);
const OUTCOMES = new Set(["succeeded", "failed"]);

function exactKeys(value, expected) {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const keys = Object.keys(value).sort();
  return keys.length === expected.length && keys.every((key, i) => key === expected[i]);
}

function validDay(day) {
  if (typeof day !== "string" || !DAY_RE.test(day)) return false;
  const parsed = new Date(`${day}T00:00:00.000Z`);
  return !Number.isNaN(parsed.valueOf()) && parsed.toISOString().slice(0, 10) === day;
}

export function validateBatch(value) {
  if (!exactKeys(value, ["events", "schema", "source"])) return "invalid envelope";
  if (value.schema !== "agentscm.telemetry.batch/v1") return "unsupported schema";
  if (!exactKeys(value.source, ["id", "kind"])) return "invalid source";
  if (!SOURCE_KINDS.has(value.source.kind) || typeof value.source.id !== "string" || !ID_RE.test(value.source.id)) {
    return "invalid source";
  }
  if (!Array.isArray(value.events) || value.events.length < 1 || value.events.length > MAX_EVENTS) {
    return "invalid events";
  }
  const eventIds = new Set();
  const eventKeys = ["arch", "count", "day", "event_id", "feature", "os", "outcome", "version"];
  for (const event of value.events) {
    if (!exactKeys(event, eventKeys)) return "invalid event";
    if (typeof event.event_id !== "string" || !ID_RE.test(event.event_id) || eventIds.has(event.event_id)) {
      return "invalid event id";
    }
    eventIds.add(event.event_id);
    if (!validDay(event.day) || typeof event.version !== "string" || event.version.length > 64 || !VERSION_RE.test(event.version)) {
      return "invalid event metadata";
    }
    if (!OS_VALUES.has(event.os) || !ARCH_VALUES.has(event.arch)) return "invalid platform";
    if (!FEATURES.has(event.feature) || !OUTCOMES.has(event.outcome)) return "invalid event value";
    if (event.count !== 1) {
      return "invalid count";
    }
  }
  return null;
}

function eventUuid(id) {
  return `${id.slice(0, 8)}-${id.slice(8, 12)}-${id.slice(12, 16)}-${id.slice(16, 20)}-${id.slice(20)}`;
}

export function toPostHogBatch(batch, projectToken) {
  return {
    token: projectToken,
    batch: batch.events.map((event) => ({
      event: "agentscm usage",
      timestamp: `${event.day}T00:00:00.000Z`,
      uuid: eventUuid(event.event_id),
      properties: {
        $geoip_disable: true,
        $insert_id: event.event_id,
        $process_person_profile: false,
        distinct_id: `${batch.source.kind}:${batch.source.id}`,
        schema: batch.schema,
        source_kind: batch.source.kind,
        day: event.day,
        version: event.version,
        os: event.os,
        arch: event.arch,
        feature: event.feature,
        outcome: event.outcome,
        count: event.count,
      },
    })),
  };
}

function parseRetryAfter(value) {
  if (typeof value !== "string" || !/^\d{1,4}$/.test(value)) return undefined;
  return String(Math.min(Number(value), 3600));
}

export function createRateLimiter({ globalPerMinute = 600, sourcePerMinute = 60, maxSources = 600, now = Date.now } = {}) {
  let windowStart = now();
  let globalCount = 0;
  const sources = new Map();

  return (sourceKey) => {
    const current = now();
    if (current - windowStart >= 60_000) {
      windowStart = current;
      globalCount = 0;
      sources.clear();
    }
    if (globalCount >= globalPerMinute) return false;
    globalCount += 1;
    if (!sources.has(sourceKey) && sources.size >= maxSources) return false;
    const sourceCount = (sources.get(sourceKey) ?? 0) + 1;
    sources.set(sourceKey, sourceCount);
    return globalCount <= globalPerMinute && sourceCount <= sourcePerMinute;
  };
}

function readBody(req, timeoutMs) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    let settled = false;
    const cleanup = () => {
      clearTimeout(timer);
      req.off("data", onData);
      req.off("end", onEnd);
      req.off("error", onError);
    };
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      cleanup();
      callback(value);
    };
    const onData = (chunk) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        req.pause();
        finish(reject, Object.assign(new Error("too large"), { statusCode: 413 }));
        return;
      }
      chunks.push(chunk);
    };
    const onEnd = () => finish(resolve, Buffer.concat(chunks));
    const onError = (error) => finish(reject, error);
    const timer = setTimeout(() => {
      req.pause();
      finish(reject, Object.assign(new Error("request timeout"), { statusCode: 408 }));
    }, timeoutMs);
    req.on("data", onData);
    req.on("end", onEnd);
    req.on("error", onError);
  });
}

function readUpstreamBody(response) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    response.on("data", (chunk) => {
      size += chunk.length;
      if (size > UPSTREAM_BODY_BYTES) {
        reject(new Error("upstream response too large"));
        response.destroy();
        return;
      }
      chunks.push(chunk);
    });
    response.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    response.on("error", reject);
  });
}

function postJson(payload, url, timeoutMs) {
  const body = Buffer.from(JSON.stringify(payload));
  const client = url.protocol === "https:" ? https : http;
  return new Promise((resolve, reject) => {
    let settled = false;
    let request;
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      callback(value);
    };
    const timer = setTimeout(() => request.destroy(new Error("backend timeout")), timeoutMs);
    request = client.request(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "content-length": body.length,
      },
      agent: false,
    }, async (response) => {
      try {
        const responseBody = await readUpstreamBody(response);
        finish(resolve, { status: response.statusCode ?? 502, headers: response.headers, body: responseBody });
      } catch (error) {
        finish(reject, error);
      }
    });
    request.on("error", (error) => finish(reject, error));
    request.end(body);
  });
}

function sendJson(res, statusCode, value, headers = {}) {
  const body = JSON.stringify(value);
  res.writeHead(statusCode, {
    "content-type": "application/json; charset=utf-8",
    "content-length": Buffer.byteLength(body),
    "cache-control": "no-store",
    ...headers,
  });
  res.end(body);
}

function acceptedByPostHog(result) {
  if (result.status !== 200) return false;
  try {
    const body = JSON.parse(result.body);
    return body.status === "Ok" && !(Array.isArray(body.quota_limited) && body.quota_limited.length > 0);
  } catch {
    return false;
  }
}

function quotaLimitedByPostHog(result) {
  if (result.status !== 200) return false;
  try {
    const body = JSON.parse(result.body);
    return Array.isArray(body.quota_limited) && body.quota_limited.length > 0;
  } catch {
    return false;
  }
}

function rejectRequest(req, res, statusCode, value, headers = {}) {
  res.once("finish", () => req.destroy());
  req.resume();
  sendJson(res, statusCode, value, { connection: "close", ...headers });
}

function buildHandler({
  projectToken,
  sendBackend,
  requestBodyTimeoutMs = 3_000,
  maxConcurrent = 64,
  limiter = createRateLimiter(),
}) {
  if (typeof projectToken !== "string" || projectToken.length < 1 || projectToken.length > 512) {
    throw new Error("a PostHog project token is required");
  }

  let active = 0;
  const handle = async (req, res) => {
    if (req.url !== "/v1/telemetry/batches") {
      rejectRequest(req, res, 404, { error: "not found" });
      return;
    }
    if (req.method !== "POST") {
      rejectRequest(req, res, 405, { error: "method not allowed" }, { allow: "POST" });
      return;
    }
    if (req.headers["content-encoding"] && req.headers["content-encoding"] !== "identity") {
      rejectRequest(req, res, 415, { error: "content encoding not supported" });
      return;
    }
    if (!/^application\/json(?:\s*;\s*charset=utf-8)?$/i.test(req.headers["content-type"] ?? "")) {
      rejectRequest(req, res, 415, { error: "content type must be application/json" });
      return;
    }
    const declaredLength = Number(req.headers["content-length"] ?? 0);
    if (!Number.isSafeInteger(declaredLength) || declaredLength < 1 || declaredLength > MAX_BODY_BYTES) {
      rejectRequest(req, res, declaredLength > MAX_BODY_BYTES ? 413 : 400, { error: "invalid content length" });
      return;
    }

    let body;
    try {
      body = await readBody(req, requestBodyTimeoutMs);
    } catch (error) {
      if (!res.headersSent) {
        rejectRequest(req, res, error.statusCode ?? 400, { error: "invalid request body" });
      }
      return;
    }
    let batch;
    try {
      batch = JSON.parse(body.toString("utf8"));
    } catch {
      sendJson(res, 400, { error: "invalid JSON" });
      return;
    }
    const validationError = validateBatch(batch);
    if (validationError) {
      sendJson(res, 400, { error: validationError });
      return;
    }
    if (!limiter(`${batch.source.kind}:${batch.source.id}`)) {
      sendJson(res, 429, { error: "rate limited" }, { "retry-after": "60" });
      return;
    }

    let result;
    try {
      result = await sendBackend(toPostHogBatch(batch, projectToken));
    } catch {
      sendJson(res, 503, { error: "analytics backend unavailable" }, { "retry-after": "5" });
      return;
    }
    if (acceptedByPostHog(result)) {
      sendJson(res, 202, { status: "accepted" });
      return;
    }
    if (result.status === 429 || result.status >= 500) {
      const retryAfter = parseRetryAfter(result.headers["retry-after"]);
      sendJson(res, 503, { error: "analytics backend unavailable" }, retryAfter ? { "retry-after": retryAfter } : {});
      return;
    }
    if (quotaLimitedByPostHog(result)) {
      sendJson(res, 429, { error: "analytics quota limited" }, { "retry-after": "3600" });
      return;
    }
    sendJson(res, 502, { error: "analytics backend rejected request" });
  };

  return async (req, res) => {
    if (active >= maxConcurrent) {
      rejectRequest(req, res, 503, { error: "ingress busy" }, { "retry-after": "1" });
      return;
    }
    active += 1;
    try {
      await handle(req, res);
    } finally {
      active -= 1;
    }
  };
}

export function createHandler({ projectToken, timeoutMs = 2_000, ...limits }) {
  return buildHandler({
    projectToken,
    ...limits,
    sendBackend: (payload) => postJson(payload, POSTHOG_URL, timeoutMs),
  });
}

export function createTestHandler({ projectToken, backendUrl, timeoutMs = 2_000, ...limits }) {
  if (!(backendUrl instanceof URL)
      || backendUrl.protocol !== "http:"
      || (backendUrl.hostname !== "127.0.0.1" && backendUrl.hostname !== "localhost")) {
    throw new Error("test backend must be loopback HTTP");
  }
  return buildHandler({
    projectToken,
    ...limits,
    sendBackend: (payload) => postJson(payload, backendUrl, timeoutMs),
  });
}

export function createIngressServer(handler, { maxConnections = 64, maxHeaderBytes = 8 * 1024 } = {}) {
  const server = http.createServer({ maxHeaderSize: maxHeaderBytes }, handler);
  server.maxConnections = maxConnections;
  server.maxHeadersCount = 32;
  server.requestTimeout = 5_000;
  server.headersTimeout = 3_000;
  server.keepAliveTimeout = 1_000;
  server.maxRequestsPerSocket = 100;
  return server;
}

export function startFromEnvironment(env = process.env) {
  if (env.AGENTSCM_TELEMETRY_INGRESS_ENABLED !== "1") {
    throw new Error("telemetry ingress is disabled; set AGENTSCM_TELEMETRY_INGRESS_ENABLED=1 explicitly");
  }
  const projectToken = env.POSTHOG_PROJECT_TOKEN;
  const host = env.AGENTSCM_TELEMETRY_INGRESS_HOST ?? "127.0.0.1";
  const port = Number(env.AGENTSCM_TELEMETRY_INGRESS_PORT ?? "8080");
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error("invalid ingress port");
  const server = createIngressServer(createHandler({ projectToken }));
  server.listen(port, host);
  return server;
}

if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) {
  try {
    startFromEnvironment();
  } catch (error) {
    process.stderr.write(`telemetry ingress did not start: ${error.message}\n`);
    process.exitCode = 1;
  }
}
