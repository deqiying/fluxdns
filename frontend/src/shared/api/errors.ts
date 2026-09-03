import type { ErrorEnvelope } from "./types";

export type ApiErrorKind =
  | "http"
  | "network"
  | "timeout"
  | "cancelled"
  | "invalid-response";

interface ApiErrorOptions {
  code: string;
  message: string;
  kind: ApiErrorKind;
  status?: number;
  requestId?: string;
  retryable?: boolean;
  retryAfterMs?: number;
  cause?: unknown;
}

export class ApiError extends Error {
  readonly code: string;
  readonly kind: ApiErrorKind;
  readonly status?: number;
  readonly requestId?: string;
  readonly retryable: boolean;
  readonly retryAfterMs?: number;

  constructor(options: ApiErrorOptions) {
    super(options.message, { cause: options.cause });
    this.name = "ApiError";
    this.code = options.code;
    this.kind = options.kind;
    this.status = options.status;
    this.requestId = options.requestId;
    this.retryable = options.retryable ?? false;
    this.retryAfterMs = options.retryAfterMs;
  }
}

export function isErrorEnvelope(value: unknown): value is ErrorEnvelope {
  if (typeof value !== "object" || value === null) {
    return false;
  }

  const candidate = value as Partial<ErrorEnvelope>;
  return (
    typeof candidate.code === "string" &&
    typeof candidate.message === "string" &&
    typeof candidate.request_id === "string" &&
    typeof candidate.retryable === "boolean"
  );
}

const errorMessages: Record<string, string> = {
  AUTH_INVALID_CREDENTIALS: "用户名或密码不正确。",
  AUTH_SESSION_EXPIRED: "登录状态已过期，请重新登录。",
  AUTH_REQUIRED: "请先登录后再访问。",
  FORBIDDEN: "当前账号无权读取此内容。",
  INVALID_ARGUMENT: "查询条件不符合服务端限制。",
  RATE_LIMITED: "请求过于频繁，请稍后重试。",
  REQUEST_TIMEOUT: "请求超时，请检查服务状态后重试。",
  NETWORK_ERROR: "无法连接管理服务，请检查服务是否正在运行。",
  INVALID_RESPONSE: "管理服务返回了无法识别的响应。",
  SERVICE_UNAVAILABLE: "管理服务暂时不可用，请稍后重试。",
};

/** 将服务端错误码映射为固定安全文案，避免直接渲染任意后端字符串。 */
export function getSafeErrorMessage(error: unknown): string {
  if (!(error instanceof ApiError)) {
    return "请求失败，请稍后重试。";
  }

  return errorMessages[error.code] ?? errorMessages[defaultCodeForError(error)] ?? "请求失败，请稍后重试。";
}

function defaultCodeForError(error: ApiError): string {
  if (error.kind === "timeout") return "REQUEST_TIMEOUT";
  if (error.kind === "network") return "NETWORK_ERROR";
  if (error.kind === "invalid-response") return "INVALID_RESPONSE";
  if (error.status === 403) return "FORBIDDEN";
  if (error.status === 429) return "RATE_LIMITED";
  if (error.status !== undefined && error.status >= 500) return "SERVICE_UNAVAILABLE";
  return error.code;
}
