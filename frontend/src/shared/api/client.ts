import { ApiError, isErrorEnvelope } from "./errors";
import type { AuthSession, Session } from "./types";

const API_V1_PREFIX = "/api/v1";
const API_V2_PREFIX = "/api/v2";
const DEFAULT_TIMEOUT_MS = 10_000;

type UnauthorizedListener = () => void;
const unauthorizedListeners = new Set<UnauthorizedListener>();
const authSessionListeners = new Set<() => void>();
let access: { token: string; expiresAt: number } | undefined;
let authEpoch = 0;
let refreshAllowed = true;
let refreshing: { epoch: number; promise: Promise<string> } | undefined;

export interface ApiRequestOptions {
  method?: "GET" | "POST";
  body?: unknown;
  signal?: AbortSignal;
  timeoutMs?: number;
  handleUnauthorized?: boolean;
  auth?: "required" | "public" | "refresh" | "logout";
}

/** 凭据只在模块内存中保存；登出或 401 同时使先前的在途刷新结果失效。 */
export function clearAccessSession(allowRefresh = false): void {
  access = undefined;
  authEpoch += 1;
  refreshAllowed = allowRefresh;
  refreshing = undefined;
  authSessionListeners.forEach((listener) => listener());
}

function installAccessSession(value: AuthSession): Session {
  if (value?.token_type !== "Bearer" || !/^[A-Za-z0-9_-]{43}$/.test(value.access_token)
      || !Number.isSafeInteger(value.access_expires_at_ms) || value.access_expires_at_ms <= 0
      || typeof value.session?.user?.name !== "string" || !value.session.user.name
      || typeof value.session.expires_at !== "string" || !Number.isFinite(Date.parse(value.session.expires_at))) {
    throw new ApiError({ code: "INVALID_RESPONSE", message: "invalid authentication response", kind: "invalid-response" });
  }
  access = { token: value.access_token, expiresAt: value.access_expires_at_ms };
  return { user: { name: value.session.user.name }, expires_at: value.session.expires_at };
}

/** 登录/初始化消费认证专用响应，只向 AuthProvider/查询缓存返回无 token 的 session。 */
export function acceptAuthSession(value: AuthSession): Session {
  clearAccessSession(true);
  return installAccessSession(value);
}

async function accessForRequest(signal: AbortSignal): Promise<{ token: string; epoch: number }> {
  const epoch = authEpoch;
  if (!refreshAllowed) throw new ApiError({ code: "AUTH_REQUIRED", message: "session required", kind: "http", status: 401 });
  if (access && access.expiresAt > Date.now() + 30_000) return { token: access.token, epoch };
  if (!refreshing || refreshing.epoch !== epoch) {
    const promise = apiRequest<AuthSession>("/auth/refresh", {
      method: "POST", auth: "refresh", timeoutMs: 5_000, handleUnauthorized: false,
    }).then((value) => {
      if (epoch !== authEpoch) throw new ApiError({ code: "REQUEST_CANCELLED", message: "session changed", kind: "cancelled" });
      installAccessSession(value);
      return value.access_token;
    }).finally(() => {
      if (refreshing?.epoch === epoch) refreshing = undefined;
    });
    refreshing = { epoch, promise };
  }
  // 一个调用取消不终止其他请求共享的刷新；每个等待方仍受自身 deadline/AbortSignal 限制。
  let onAbort: () => void = () => {};
  const cancelled = new Promise<never>((_, reject) => {
    onAbort = () => reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    if (signal.aborted) onAbort();
    else signal.addEventListener("abort", onAbort, { once: true });
  });
  try {
    return { token: await Promise.race([refreshing.promise, cancelled]), epoch };
  } finally {
    signal.removeEventListener("abort", onAbort);
  }
}

/** 订阅非鉴权接口的 401；AuthProvider 负责统一清理内存 session 和跳转。 */
export function onUnauthorized(listener: UnauthorizedListener): () => void {
  unauthorizedListeners.add(listener);
  return () => unauthorizedListeners.delete(listener);
}

/** WS 等进程内资源监听认证世代变化，确保登出和重新登录不会复用旧连接。 */
export function onAuthSessionChange(listener: () => void): () => void {
  authSessionListeners.add(listener);
  return () => authSessionListeners.delete(listener);
}

/** 非 HTTP transport 报告同一认证失效事件，不另建第二套登录状态。 */
export function reportUnauthorized(): void {
  clearAccessSession();
  unauthorizedListeners.forEach((listener) => listener());
}

export async function apiRequest<T>(path: string, options: ApiRequestOptions = {}): Promise<T> {
  return requestWithPrefix<T>(API_V1_PREFIX, path, options);
}

/** 新版配置和管理能力的同源入口；不会改变现有 v1 页面或认证刷新路径。 */
export async function apiV2Request<T>(path: string, options: ApiRequestOptions = {}): Promise<T> {
  return requestWithPrefix<T>(API_V2_PREFIX, path, options);
}

async function requestWithPrefix<T>(prefix: string, path: string, options: ApiRequestOptions): Promise<T> {
  const controller = new AbortController();
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  let timedOut = false;
  const mode = options.auth ?? "required";
  let requestEpoch = authEpoch;

  const abortFromCaller = () => controller.abort(options.signal?.reason);
  if (options.signal?.aborted) {
    abortFromCaller();
  } else {
    options.signal?.addEventListener("abort", abortFromCaller, { once: true });
  }

  const timeout = window.setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);

  try {
    controller.signal.throwIfAborted();
    const authorization = mode === "required" || mode === "logout" ? await accessForRequest(controller.signal) : undefined;
    if (authorization) {
      requestEpoch = authorization.epoch;
      if (requestEpoch !== authEpoch) throw new ApiError({ code: "REQUEST_CANCELLED", message: "session changed", kind: "cancelled" });
    }
    controller.signal.throwIfAborted();
    const response = await fetch(`${prefix}${path.startsWith("/") ? path : `/${path}`}`, {
      method: options.method ?? "GET",
      credentials: mode === "required" ? "omit" : "same-origin",
      headers: {
        Accept: "application/json",
        ...(authorization ? { Authorization: `Bearer ${authorization.token}` } : {}),
        ...(options.body === undefined ? {} : { "Content-Type": "application/json" }),
      },
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: controller.signal,
    });

    if (response.status === 204) {
      return undefined as T;
    }

    const contentType = response.headers.get("content-type") ?? "";
    if (!contentType.toLowerCase().includes("application/json")) {
      throw new ApiError({
        code: "INVALID_RESPONSE",
        message: "expected application/json response",
        kind: "invalid-response",
        status: response.status,
        requestId: response.headers.get("x-request-id") ?? undefined,
      });
    }

    let payload: unknown;
    try {
      payload = await response.json();
    } catch (cause) {
      throw new ApiError({
        code: "INVALID_RESPONSE",
        message: "response body is not valid JSON",
        kind: "invalid-response",
        status: response.status,
        requestId: response.headers.get("x-request-id") ?? undefined,
        cause,
      });
    }

    if (!response.ok) {
      const envelope = isErrorEnvelope(payload) ? payload : undefined;
      const error = new ApiError({
        code: envelope?.code ?? defaultHttpErrorCode(response.status),
        message: envelope?.message ?? `management API returned HTTP ${response.status}`,
        kind: "http",
        status: response.status,
        requestId: envelope?.request_id ?? response.headers.get("x-request-id") ?? undefined,
        retryable: envelope?.retryable ?? (response.status === 429 || response.status >= 500),
        retryAfterMs: parseRetryAfter(response.headers.get("retry-after")),
        fieldErrors: extractFieldErrors(payload),
      });

      throw error;
    }

    return payload as T;
  } catch (error) {
    if (error instanceof ApiError) {
      if (error.status === 401 && (mode === "required" || mode === "logout") && requestEpoch === authEpoch) {
        if (options.handleUnauthorized === false) clearAccessSession();
        else reportUnauthorized();
      }
      throw error;
    }
    if (controller.signal.aborted) {
      throw new ApiError({
        code: timedOut ? "REQUEST_TIMEOUT" : "REQUEST_CANCELLED",
        message: timedOut ? "request timed out" : "request was cancelled",
        kind: timedOut ? "timeout" : "cancelled",
        retryable: timedOut,
        cause: error,
      });
    }
    throw new ApiError({
      code: "NETWORK_ERROR",
      message: "management API network request failed",
      kind: "network",
      retryable: true,
      cause: error,
    });
  } finally {
    window.clearTimeout(timeout);
    options.signal?.removeEventListener("abort", abortFromCaller);
  }
}

export function createSearchParams(values: Record<string, string | number | undefined>): string {
  const params = new URLSearchParams();
  Object.entries(values).forEach(([key, value]) => {
    if (value !== undefined && value !== "") {
      params.set(key, String(value));
    }
  });
  return params.toString();
}

function defaultHttpErrorCode(status: number): string {
  if (status === 401) return "AUTH_REQUIRED";
  if (status === 403) return "FORBIDDEN";
  if (status === 429) return "RATE_LIMITED";
  if (status >= 500) return "SERVICE_UNAVAILABLE";
  return "INVALID_ARGUMENT";
}

function parseRetryAfter(value: string | null): number | undefined {
  if (value === null) return undefined;
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds > 0 ? seconds * 1_000 : undefined;
}

function extractFieldErrors(value: unknown): { path: string; code: string }[] {
  if (typeof value !== "object" || value === null || !("field_errors" in value)) return [];
  const fieldErrors = (value as { field_errors?: unknown }).field_errors;
  if (!Array.isArray(fieldErrors)) return [];
  return fieldErrors.flatMap((item) =>
    typeof item === "object" && item !== null
      && typeof (item as { path?: unknown }).path === "string"
      && typeof (item as { code?: unknown }).code === "string"
      ? [{ path: (item as { path: string }).path, code: (item as { code: string }).code }]
      : [],
  );
}
