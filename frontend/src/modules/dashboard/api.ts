import type { components } from "@/shared/api/generated-v2";
import { apiV2Request } from "@/shared/api/client";

export type ServiceMetrics = components["schemas"]["ServiceMetrics"];
export const serviceMetricsKey = ["api", "v2", "service", "metrics"] as const;

export function getServiceMetrics(signal?: AbortSignal): Promise<ServiceMetrics> {
  return apiV2Request<ServiceMetrics>("/service/metrics", { signal });
}
