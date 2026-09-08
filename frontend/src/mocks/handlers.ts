import { http, HttpResponse } from "msw";
import type { AuthSession } from "@/shared/api/types";
import {
  healthFixture,
  hostsConfigReadFixture,
  configStateFixture,
  dnsConfigReadFixture,
  logsConfigReadFixture,
  listenersConfigReadFixture,
  overviewFixture,
  outboundConfigReadFixture,
  processMetricsFixture,
  queryPageFixture,
  ruleSetsConfigReadFixture,
  retentionStatusFixture,
  resourceFixture,
  runtimeFixture,
  sessionFixture,
  serviceMetricsFixture,
  setupReadyFixture,
  setupRequiredFixture,
  statisticsConfigReadFixture,
  statisticsFixture,
  strategiesConfigReadFixture,
  systemConfigReadFixture,
  systemFixture,
  upstreamsConfigReadFixture,
  v2QueryPageFixture,
  v2QueryRecordsFixture,
} from "./fixtures";

let authenticated = false;
let setupRequired = false;
let authenticatedName = sessionFixture.user.name;
const MOCK_ACCESS_TOKEN = "A".repeat(43);

function authorized(request: Request): boolean {
  return authenticated && request.headers.get("authorization") === `Bearer ${MOCK_ACCESS_TOKEN}`;
}

function authSession(): AuthSession {
  return {
    session: { ...sessionFixture, user: { name: authenticatedName } },
    access_token: MOCK_ACCESS_TOKEN,
    token_type: "Bearer",
    access_expires_at_ms: Date.now() + 300_000,
  };
}

export function setMockAuthenticated(value: boolean) {
  authenticated = value;
}

export function setMockSetupRequired(value: boolean) {
  setupRequired = value;
  if (value) authenticated = false;
}

export function resetMockState() {
  authenticated = false;
  setupRequired = false;
  authenticatedName = sessionFixture.user.name;
}

function unauthorized() {
  return HttpResponse.json(
    { code: "AUTH_REQUIRED", message: "session required", request_id: "mock-auth-401", retryable: false },
    { status: 401, headers: { "X-Request-Id": "mock-auth-401" } },
  );
}

function invalidArgument(message: string) {
  return HttpResponse.json(
    { code: "INVALID_ARGUMENT", message, request_id: "mock-invalid-400", retryable: false },
    { status: 400, headers: { "X-Request-Id": "mock-invalid-400" } },
  );
}

function v2Error(status: 400 | 401 | 404, code: "INVALID_ARGUMENT" | "AUTH_REQUIRED" | "NOT_FOUND", message: string) {
  const requestId = `mock-v2-${status}`;
  return HttpResponse.json(
    { code, message, request_id: requestId, retryable: false, field_errors: [] },
    { status, headers: { "X-Request-Id": requestId } },
  );
}

function readOnly<T extends object>(fixture: T) {
  return ({ request }: { request: Request }) => (authorized(request) ? HttpResponse.json(fixture) : unauthorized());
}

function readOnlyV2<T extends object>(fixture: T) {
  return ({ request }: { request: Request }) => (
    authorized(request) ? HttpResponse.json(fixture) : v2Error(401, "AUTH_REQUIRED", "session required")
  );
}

export const handlers = [
  http.get("/api/v1/auth/setup", () => HttpResponse.json(setupRequired ? setupRequiredFixture : setupReadyFixture)),
  http.get("/api/v1/auth/session", ({ request }) => (authorized(request) ? HttpResponse.json(authSession().session) : unauthorized())),
  // mock 的 authenticated 仅模拟浏览器刷新会话；真实 Cookie/Origin 防护由后端与浏览器联测验证。
  http.post("/api/v1/auth/refresh", () => (authenticated ? HttpResponse.json(authSession()) : unauthorized())),
  http.post("/api/v1/auth/setup", async ({ request }) => {
    if (!setupRequired) {
      return HttpResponse.json(
        { code: "SETUP_ALREADY_COMPLETED", message: "setup already completed", request_id: "mock-setup-409", retryable: false },
        { status: 409 },
      );
    }
    const body = (await request.json()) as { username?: string; password?: string };
    if (!body.username || !body.password || body.password.length < 12) {
      return HttpResponse.json(
        { code: "VALIDATION_FAILED", message: "invalid setup credentials", request_id: "mock-setup-400", retryable: false },
        { status: 400 },
      );
    }
    setupRequired = false;
    authenticated = true;
    authenticatedName = body.username;
    return HttpResponse.json(authSession(), { status: 201 });
  }),
  http.post("/api/v1/auth/login", async ({ request }) => {
    const body = (await request.json()) as { username?: string; password?: string };
    if (!body.username || !body.password) {
      return HttpResponse.json(
        { code: "AUTH_INVALID_CREDENTIALS", message: "invalid credentials", request_id: "mock-login-401", retryable: false },
        { status: 401 },
      );
    }
    authenticated = true;
    authenticatedName = body.username;
    return HttpResponse.json(authSession());
  }),
  http.post("/api/v1/auth/logout", ({ request }) => {
    if (authorized(request)) authenticated = false;
    return new HttpResponse(null, { status: 204 });
  }),
  http.get("/api/v1/overview", readOnly(overviewFixture)),
  http.get("/api/v1/runtime", readOnly(runtimeFixture)),
  http.get("/api/v1/health", readOnly(healthFixture)),
  http.get("/api/v1/statistics", ({ request }) => {
    if (!authorized(request)) return unauthorized();
    const url = new URL(request.url);
    const page = Number(url.searchParams.get("page") ?? "1");
    const pageSize = Number(url.searchParams.get("page_size") ?? "20");
    if (page < 1 || pageSize < 1 || pageSize > 100) return invalidArgument("pagination outside contract");
    const dimension = url.searchParams.get("dimension") ?? "total";
    const items = statisticsFixture.items.map((item) => ({ ...item, dimension_kind: dimension, dimension_value: dimension === "total" ? "all" : "fixture" }));
    return HttpResponse.json({ ...statisticsFixture, page, page_size: pageSize, items });
  }),
  http.get("/api/v1/queries", ({ request }) => {
    if (!authorized(request)) return unauthorized();
    const url = new URL(request.url);
    const page = Number(url.searchParams.get("page") ?? "1");
    const pageSize = Number(url.searchParams.get("page_size") ?? "20");
    if (page < 1 || pageSize < 1 || pageSize > 100) return invalidArgument("pagination outside contract");
    const start = (page - 1) * pageSize;
    const items = queryPageFixture.items.slice(start, start + pageSize);
    return HttpResponse.json({ ...queryPageFixture, page, page_size: pageSize, items });
  }),
  http.get("/api/v1/resources", readOnly(resourceFixture)),
  http.get("/api/v1/system", readOnly(systemFixture)),
  http.get("/api/v2/system/runtime", readOnlyV2(processMetricsFixture)),
  http.get("/api/v2/service/metrics", readOnlyV2(serviceMetricsFixture)),
  http.get("/api/v2/config/state", readOnlyV2(configStateFixture)),
  http.get("/api/v2/config/system", readOnlyV2(systemConfigReadFixture)),
  http.get("/api/v2/config/modules/:module", ({ request, params }) => {
    if (!authorized(request)) return v2Error(401, "AUTH_REQUIRED", "session required");
    const fixture = {
      dns: dnsConfigReadFixture,
      hosts: hostsConfigReadFixture,
      listener: listenersConfigReadFixture,
      statistics: statisticsConfigReadFixture,
      strategy: strategiesConfigReadFixture,
      logs: logsConfigReadFixture,
      outbound: outboundConfigReadFixture,
      rule_set: ruleSetsConfigReadFixture,
      upstreams: upstreamsConfigReadFixture,
    }[String(params.module)];
    return fixture
      ? HttpResponse.json(fixture)
      : v2Error(404, "NOT_FOUND", "module fixture not found");
  }),
  http.post("/api/v2/config/modules/:module/validate", async ({ request, params }) => {
    if (!authorized(request)) return v2Error(401, "AUTH_REQUIRED", "session required");
    const candidate = await request.json() as { expected?: unknown; changes?: Array<{ module?: string }> };
    if (candidate.changes?.length !== 1 || candidate.changes[0]?.module !== String(params.module)) {
      return v2Error(400, "INVALID_ARGUMENT", "module candidate mismatch");
    }
    return HttpResponse.json({
      validation_token: "validation-1",
      expected: candidate.expected,
      expires_at_ms: Date.now() + 30_000,
      required_confirmations: [],
      affected_names: [],
    });
  }),
  http.post("/api/v2/config/modules/:module/apply", async ({ request, params }) => {
    if (!authorized(request)) return v2Error(401, "AUTH_REQUIRED", "session required");
    const body = await request.json() as {
      operation_id?: string;
      candidate?: { changes?: Array<{ module?: string }> };
    };
    if (body.candidate?.changes?.length !== 1 || body.candidate.changes[0]?.module !== String(params.module)) {
      return v2Error(400, "INVALID_ARGUMENT", "module apply mismatch");
    }
    return HttpResponse.json({
      operation_id: body.operation_id,
      status: { state: "applied_synced", active_revision: "active-9", persisted_revision: "active-9" },
    });
  }),
  http.get("/api/v2/retention", readOnlyV2(retentionStatusFixture)),
  http.post("/api/v2/queries/search", async ({ request }) => {
    if (!authorized(request)) return v2Error(401, "AUTH_REQUIRED", "session required");
    const body = await request.json() as { filter?: { from_ms?: number; to_ms?: number }; page_size?: number };
    if (!body.filter || !Number.isSafeInteger(body.filter.from_ms) || !Number.isSafeInteger(body.filter.to_ms)) {
      return v2Error(400, "INVALID_ARGUMENT", "query time range is required");
    }
    if (!Number.isSafeInteger(body.page_size) || Number(body.page_size) < 1 || Number(body.page_size) > 100) {
      return v2Error(400, "INVALID_ARGUMENT", "page size outside contract");
    }
    return HttpResponse.json({ ...v2QueryPageFixture, items: v2QueryPageFixture.items.slice(0, Number(body.page_size)) });
  }),
  http.get("/api/v2/queries/:recordId", ({ request, params }) => {
    if (!authorized(request)) return v2Error(401, "AUTH_REQUIRED", "session required");
    const record = v2QueryRecordsFixture.find(({ id }) => id === params.recordId);
    return record
      ? HttpResponse.json({ record, directory_revision: v2QueryPageFixture.directory_revision })
      : v2Error(404, "NOT_FOUND", "record fixture not found");
  }),
];
