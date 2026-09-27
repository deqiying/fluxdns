import { ApiError, isErrorEnvelope } from "./errors";
import type { AuthSession, Session } from "./types";

const API_V2_PREFIX = "/api/v2";
const DEFAULT_TIMEOUT_MS = 10_000;
const REFRESH_TIMEOUT_MS = 5_000;
/** 提前换发阈值；必须小于后端 renew 窗口，否则服务端不会真正换发新凭据。 */
const ACCESS_RENEW_MARGIN_MS = 120_000;
/** 后台保活周期：界面长时间无请求时，同源刷新同时维持服务端会话活跃时间。 */
export const SESSION_KEEPALIVE_INTERVAL_MS = 10 * 60_000;

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

type AuthMode = NonNullable<ApiRequestOptions["auth"]>;

/** 单次尝试的会话世代与失败归因，供 requestWithPrefix 判断 401 是否仍属当前登录态、是否值得重放。 */
interface Attempt {
  epoch: number;
  /** 本次 401 是否来自换发访问凭据失败：此时重放没有意义。 */
  refreshFailed: boolean;
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

/**
 * 取得业务请求可用的访问凭据。
 * force 表示调用方已确认当前凭据被服务端拒绝，必须真正换发而不能复用内存值。
 */
async function acquireAccess(
  signal: AbortSignal,
  force = false,
): Promise<{ token: string; epoch: number }> {
  const epoch = authEpoch;
  if (!refreshAllowed) throw new ApiError({ code: "AUTH_REQUIRED", message: "session required", kind: "http", status: 401 });
  if (!force && access && access.expiresAt > Date.now() + ACCESS_RENEW_MARGIN_MS) return { token: access.token, epoch };
  if (!refreshing || refreshing.epoch !== epoch) {
    const promise = apiV2Request<AuthSession>("/auth/refresh", {
      method: "POST", auth: "refresh", timeoutMs: REFRESH_TIMEOUT_MS,
    }).then((value) => {
      if (epoch !== authEpoch) throw new ApiError({ code: "REQUEST_CANCELLED", message: "session changed", kind: "cancelled" });
      installAccessSession(value);
      return value.access_token;
    }, (error: unknown) => {
      // 换发失败同样按代次判定：旧代次的 401 不得被当成当前会话已失效。
      if (epoch !== authEpoch) throw new ApiError({ code: "REQUEST_CANCELLED", message: "session changed", kind: "cancelled" });
      throw error;
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

/**
 * 主动换发访问凭据；同源刷新同时更新服务端会话活跃时间，因此也用于后台保活。
 * 刷新凭据失效时抛出 401，由调用方决定是否结束登录态。
 */
export async function renewAccessSession(): Promise<void> {
  await acquireAccess(new AbortController().signal, true);
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

/** 唯一 v2 同源契约入口，认证和业务共用取消及会话边界。 */
export async function apiV2Request<T>(path: string, options: ApiRequestOptions = {}): Promise<T> {
  return requestWithPrefix<T>(API_V2_PREFIX, path, options);
}

/**
 * 会话型请求的 401 处置：先用刷新凭据换发访问凭据并重放原请求，刷新凭据也失效才结束登录态。
 * 401 由服务端鉴权在业务处理前返回，重放不会重复执行写操作；配置写操作另带 operation_id 服务端去重。
 */
async function requestWithPrefix<T>(prefix: string, path: string, options: ApiRequestOptions): Promise<T> {
  const mode = options.auth ?? "required";
  const authenticated = mode === "required" || mode === "logout";
  const maxAttempts = authenticated ? 2 : 1;
  const attempt: Attempt = { epoch: authEpoch, refreshFailed: false };
  for (let count = 1; ; count += 1) {
    try {
      return await sendOnce<T>(prefix, path, options, mode, attempt);
    } catch (error) {
      if (!(error instanceof ApiError) || error.status !== 401 || !authenticated || attempt.epoch !== authEpoch) throw error;
      if (count < maxAttempts && refreshAllowed && !attempt.refreshFailed) {
        try {
          await renewAccessSession();
          continue;
        } catch (refreshError) {
          // 只有刷新凭据失效才是登录态结束；网络和超时错误原样上报，交给调用方重试。
          if (!(refreshError instanceof ApiError) || refreshError.status !== 401) throw refreshError;
        }
      }
      if (options.handleUnauthorized === false) clearAccessSession();
      else reportUnauthorized();
      throw error;
    }
  }
}

async function sendOnce<T>(prefix: string, path: string, options: ApiRequestOptions, mode: AuthMode, attempt: Attempt): Promise<T> {
  const controller = new AbortController();
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  let timedOut = false;
  attempt.epoch = authEpoch;

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
    attempt.refreshFailed = false;
    let authorization: { token: string; epoch: number } | undefined;
    if (mode === "required" || mode === "logout") {
      try {
        authorization = await acquireAccess(controller.signal);
      } catch (error) {
        // 换发失败已说明刷新凭据不可用，重放一次没有意义。
        if (error instanceof ApiError && error.status === 401) attempt.refreshFailed = true;
        throw error;
      }
    }
    if (authorization) {
      attempt.epoch = authorization.epoch;
      if (attempt.epoch !== authEpoch) throw new ApiError({ code: "REQUEST_CANCELLED", message: "session changed", kind: "cancelled" });
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
      // 401 的重放与登出决策集中在 requestWithPrefix，单次尝试不产生会话副作用。
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
