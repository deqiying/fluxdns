import { http, HttpResponse } from "msw";
import {
  healthFixture,
  overviewFixture,
  queryPageFixture,
  resourceFixture,
  runtimeFixture,
  sessionFixture,
  statisticsFixture,
  systemFixture,
} from "./fixtures";

let authenticated = false;

export function setMockAuthenticated(value: boolean) {
  authenticated = value;
}

export function resetMockState() {
  authenticated = false;
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

function readOnly<T extends object>(fixture: T) {
  return () => (authenticated ? HttpResponse.json(fixture) : unauthorized());
}

export const handlers = [
  http.get("/api/v1/auth/session", () => (authenticated ? HttpResponse.json(sessionFixture) : unauthorized())),
  http.post("/api/v1/auth/login", async ({ request }) => {
    const body = (await request.json()) as { username?: string; password?: string };
    if (!body.username || !body.password) {
      return HttpResponse.json(
        { code: "AUTH_INVALID_CREDENTIALS", message: "invalid credentials", request_id: "mock-login-401", retryable: false },
        { status: 401 },
      );
    }
    authenticated = true;
    return HttpResponse.json({ ...sessionFixture, user: { name: body.username } });
  }),
  http.post("/api/v1/auth/logout", () => {
    authenticated = false;
    return new HttpResponse(null, { status: 204 });
  }),
  http.get("/api/v1/overview", readOnly(overviewFixture)),
  http.get("/api/v1/runtime", readOnly(runtimeFixture)),
  http.get("/api/v1/health", readOnly(healthFixture)),
  http.get("/api/v1/statistics", ({ request }) => {
    if (!authenticated) return unauthorized();
    const url = new URL(request.url);
    const page = Number(url.searchParams.get("page") ?? "1");
    const pageSize = Number(url.searchParams.get("page_size") ?? "20");
    if (page < 1 || pageSize < 1 || pageSize > 100) return invalidArgument("pagination outside contract");
    const dimension = url.searchParams.get("dimension") ?? "total";
    const items = statisticsFixture.items.map((item) => ({ ...item, dimension_kind: dimension, dimension_value: dimension === "total" ? "all" : "fixture" }));
    return HttpResponse.json({ ...statisticsFixture, page, page_size: pageSize, items });
  }),
  http.get("/api/v1/queries", ({ request }) => {
    if (!authenticated) return unauthorized();
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
];
