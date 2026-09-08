import { apiRequest, apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";
import type { SystemInfo } from "@/shared/api/types";

export const systemKey = ["api", "v1", "system"] as const;
export const processMetricsKey = ["api", "v2", "system", "runtime"] as const;
export type ProcessMetrics = components["schemas"]["ProcessMetrics"];

export function getSystem(signal?: AbortSignal): Promise<SystemInfo> {
  return apiRequest<SystemInfo>("/system", { signal });
}

export function getProcessMetrics(signal?: AbortSignal): Promise<ProcessMetrics> {
  return apiV2Request<ProcessMetrics>("/system/runtime", { signal });
}
