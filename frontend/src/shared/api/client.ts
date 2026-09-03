import { ApiError, isErrorEnvelope } from "./errors";

const API_PREFIX = "/api/v1";
const DEFAULT_TIMEOUT_MS = 10_000;

type UnauthorizedListener = () => void;
const unauthorizedListeners = new Set<UnauthorizedListener>();

export interface ApiRequestOptions {
  method?: "GET" | "POST";
  body?: unknown;
  signal?: AbortSignal;
  timeoutMs?: number;
  handleUnauthorized?: boolean;
}

/** 订阅非鉴权接口的 401；AuthProvider 负责统一清理内存 session 和跳转。 */
export function onUnauthorized(listener: UnauthorizedListener): () => void {
  unauthorizedListeners.add(listener);
  return () => unauthorizedListeners.delete(listener);
}

export async function apiRequest<T>(path: string, options: ApiRequestOptions = {}): Promise<T> {
  const controller = new AbortController();
  const timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  let timedOut = false;

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
    const response = await fetch(`${API_PREFIX}${path.startsWith("/") ? path : `/${path}`}`, {
      method: options.method ?? "GET",
      credentials: "same-origin",
      headers: {
        Accept: "application/json",
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
      });

      if (response.status === 401 && options.handleUnauthorized !== false) {
        unauthorizedListeners.forEach((listener) => listener());
      }
      throw error;
    }

    return payload as T;
  } catch (error) {
    if (error instanceof ApiError) {
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
