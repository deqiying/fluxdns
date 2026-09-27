import { QueryClient } from "@tanstack/react-query";
import { ApiError } from "@/shared/api/errors";

export const SUMMARY_POLL_INTERVAL_MS = 30_000;

export function createAppQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        staleTime: 10_000,
        gcTime: 5 * 60_000,
        refetchOnWindowFocus: true,
        refetchOnReconnect: true,
        retry: shouldRetry,
        retryDelay: retryDelay,
      },
      mutations: {
        retry: false,
      },
    },
  });
}

/** 页面隐藏时返回 false，确保摘要轮询不会在后台继续占用管理面额度。 */
export function getSummaryPollInterval(visible = document.visibilityState === "visible"): number | false {
  return visible ? SUMMARY_POLL_INTERVAL_MS : false;
}

function shouldRetry(failureCount: number, error: unknown): boolean {
  if (!(error instanceof ApiError)) return failureCount < 1;
  if (error.kind === "cancelled" || error.status === 401 || error.status === 403) return false;
  return error.retryable && failureCount < 2;
}

function retryDelay(attemptIndex: number, error: unknown): number {
  if (error instanceof ApiError && error.retryAfterMs !== undefined) {
    return Math.min(error.retryAfterMs, 60_000);
  }
  return Math.min(2 ** attemptIndex * 2_000, 30_000);
}
