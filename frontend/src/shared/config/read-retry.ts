import { ApiError } from "@/shared/api/errors";

/** 仅供配置读取与操作结果轮询使用；不能据此重放配置写请求。 */
export function isTransientConfigRead(error: unknown): boolean {
  return error instanceof ApiError
    && error.kind !== "cancelled"
    && error.status !== 401 && error.status !== 403
    && (error.code === "OPERATION_BUSY" || error.retryable);
}

export function retryConfigRead(failureCount: number, error: unknown): boolean {
  return isTransientConfigRead(error) && failureCount < 3;
}

export function configReadRetryDelay(attempt: number, error: unknown): number {
  if (error instanceof ApiError && error.retryAfterMs !== undefined) return Math.min(error.retryAfterMs, 60_000);
  return Math.min(250 * 2 ** attempt, 1_000) + Math.floor(Math.random() * 100);
}
