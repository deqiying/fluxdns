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
  fieldErrors?: readonly ApiFieldError[];
  diagnostic?: unknown;
  /** 仅可传入已验证 ErrorEnvelope 的安全 message，不能使用任意 Error.message。 */
  serverMessage?: string;
  operationId?: string;
  cause?: unknown;
}

export interface ApiFieldError {
  path: string;
  code: string;
}

export class ApiError extends Error {
  readonly code: string;
  readonly kind: ApiErrorKind;
  readonly status?: number;
  readonly requestId?: string;
  readonly retryable: boolean;
  readonly retryAfterMs?: number;
  readonly fieldErrors: readonly ApiFieldError[];
  readonly diagnostic?: ErrorEnvelope["diagnostic"];
  readonly serverMessage?: string;
  readonly operationId?: string;

  constructor(options: ApiErrorOptions) {
    super(options.message, { cause: options.cause });
    this.name = "ApiError";
    this.code = safeIdentifier(options.code, 64) ?? "INVALID_RESPONSE";
    this.kind = options.kind;
    this.status = options.status;
    this.requestId = safeIdentifier(options.requestId);
    this.retryable = options.retryable ?? false;
    this.retryAfterMs = options.retryAfterMs;
    this.fieldErrors = (options.fieldErrors ?? []).slice(0, 128).flatMap((item) => {
      const path = safeText(item.path, 512);
      const code = safeIdentifier(item.code, 64);
      return path && code ? [{ path, code }] : [];
    });
    this.diagnostic = parseDiagnostic(options.diagnostic);
    this.serverMessage = safeText(options.serverMessage, 1024);
    this.operationId = safeIdentifier(options.operationId);
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
  HTTP_VERSION_NOT_SUPPORTED: "不支持此 HTTP 版本。",
  URI_TOO_LONG: "请求地址超过长度上限。",
  HEADERS_TOO_LARGE: "请求头超过大小上限。",
  INTERNAL_ERROR: "管理服务内部错误，请凭请求 ID 检查服务日志。",
  SETUP_ALREADY_COMPLETED: "服务已完成初始化，不能重复初始化。",
  ORIGIN_REJECTED: "请求来源不被允许，请核对 WebUI 访问地址。",
  PAYLOAD_TOO_LARGE: "请求内容超过大小上限。",
  VERSION_UNSUPPORTED: "不支持此配置或协议版本。",
  NOT_FOUND: "请求的资源不存在。",
  ACTIVE_REVISION_CONFLICT: "运行配置已改变，请刷新后重新操作。",
  OPERATION_ID_REUSED: "操作 ID 已绑定其他请求，不能复用。",
  CURSOR_EXPIRED: "查询游标已过期，请重新开始查询。",
  AUTH_INVALID_CREDENTIALS: "用户名或密码不正确。",
  VALIDATION_FAILED: "配置校验失败，请检查字段错误。",
  CONFIG_CONFLICT: "配置存在冲突，请核对后重试。",
  REQUEST_CANCELLED: "请求已取消；若已提交变更，请先核对操作结果。",
  AUTH_SESSION_EXPIRED: "登录状态已过期，请重新登录。",
  AUTH_REQUIRED: "请先登录后再访问。",
  FORBIDDEN: "当前账号无权读取此内容。",
  INVALID_ARGUMENT: "请求参数或格式无效，请检查输入。",
  RATE_LIMITED: "请求过于频繁，请稍后重试。",
  REQUEST_TIMEOUT: "请求超时；若已提交变更，请先查询操作结果。",
  NETWORK_ERROR: "无法连接管理服务，请检查服务是否正在运行。",
  INVALID_RESPONSE: "管理服务返回了无法识别的响应。",
  SERVICE_UNAVAILABLE: "管理服务暂时不可用，请稍后重试。",
  FILE_REVISION_CONFLICT: "配置文件已再次变化，请重新读取差异。",
  CONFIRMATION_REQUIRED: "配置影响需要重新确认；当前草稿已保留。",
  EXTERNAL_CHANGES_REQUIRE_CONFIRMATION: "外部文件变化需要确认覆盖；当前草稿已保留。",
  OPERATION_BUSY: "已有配置操作正在执行，请等待其完成。",
  CONFIG_READ_FAILED: "配置读取失败，请刷新成功后再保存；当前草稿已保留。",
  CONFIG_OPERATION_PENDING: "配置操作结果尚未确认，请在全局提示中继续查询；当前草稿已保留。",
  PERSISTENCE_FAILED: "运行配置已生效，但配置文件同步失败。",
  APPLY_FAILED: "配置未能应用，运行状态保持不变。",
  COMPENSATION_FAILED: "配置补偿失败，请先核对当前运行状态。",
} satisfies Record<ErrorEnvelope["code"], string> & Record<string, string>;

/** 保留中文语义，再附服务端安全投影和关联 ID；任意 Error/cause 永不进入 UI。 */
export function getSafeErrorMessage(error: unknown, fallback?: string): string {
  if (!(error instanceof ApiError)) return fallback ?? "请求失败，请稍后重试。";
  const base = fallback ?? errorMessages[error.code] ?? errorMessages[defaultCodeForError(error)] ?? "请求失败，请稍后重试。";
  const parts = [base];
  const diagnostic = error.diagnostic;
  if (diagnostic) {
    parts.push(`阶段：${diagnostic.stage}`, ...(diagnostic.resource ? [`资源：${diagnostic.resource}`] : []), diagnostic.message, `原因码：${diagnostic.code}`);
  } else if (error.serverMessage && !/^request could not be completed[.!]?$/i.test(error.serverMessage) && error.serverMessage !== base) {
    parts.push(error.serverMessage);
  }
  if (error.status !== undefined) parts.push(`HTTP ${error.status}`);
  if (error.requestId) parts.push(`请求 ID：${error.requestId}`);
  if (error.operationId) parts.push(`操作 ID：${error.operationId}`);
  for (const field of error.fieldErrors) parts.push(`字段 ${field.path}：${field.code}`);
  return parts.join("；");
}

/** 防御旧版/代理异常数据：按 UTF-8 字节截断，移除控制字符及双向显示控制。 */
export function safeText(value: unknown, maxBytes: number): string | undefined {
  if (typeof value !== "string") return undefined;
  const clean = value.replace(/[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u206f\ufeff]/g, " ").trim();
  let result = "";
  let bytes = 0;
  for (const char of clean) {
    bytes += new TextEncoder().encode(char).length;
    if (bytes > maxBytes) break;
    result += char;
  }
  return result || undefined;
}

function safeIdentifier(value: unknown, maxBytes = 128): string | undefined {
  if (typeof value !== "string" || !/^[A-Za-z0-9_.:~-]+$/.test(value)) return undefined;
  return safeText(value, maxBytes);
}

/** diagnostic 可选且不能使旧响应解析失败；结构不合法时退回原 code/message。 */
export function parseDiagnostic(value: unknown): ErrorEnvelope["diagnostic"] {
  if (typeof value !== "object" || value === null) return undefined;
  const item = value as Record<string, unknown>;
  const stage = safeIdentifier(item.stage, 64);
  const code = safeIdentifier(item.code, 64);
  const message = safeText(item.message, 1024);
  if (!stage || !code || !message) return undefined;
  const resource = safeText(item.resource, 128);
  return { stage, code, message, ...(resource ? { resource } : {}) };
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
