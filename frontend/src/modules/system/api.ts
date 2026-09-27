import { apiV2Request } from "@/shared/api/client";
import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];

export const processMetricsKey = ["api", "v2", "system", "runtime"] as const;
/**
 * 与服务状态页、系统配置页各自声明的 key 字面量相同：同一 URL 命中同一份 React Query 缓存，
 * 页面间跳转不会因为口径不同而重复取样。
 */
export const serviceMetricsKey = ["api", "v2", "service", "metrics"] as const;
export const systemConfigKey = ["api", "v2", "config", "system"] as const;

export type ProcessMetrics = Schemas["ProcessMetrics"];
export type HostEnvironment = Schemas["HostEnvironment"];
export type ServiceMetrics = Schemas["ServiceMetrics"];
export type SystemConfigRead = Schemas["SystemConfigRead"];

export function getProcessMetrics(signal?: AbortSignal): Promise<ProcessMetrics> {
  return apiV2Request<ProcessMetrics>("/system/runtime", { signal });
}

export function getServiceMetrics(signal?: AbortSignal): Promise<ServiceMetrics> {
  return apiV2Request<ServiceMetrics>("/service/metrics", { signal });
}

export function getSystemConfig(signal?: AbortSignal): Promise<SystemConfigRead> {
  return apiV2Request<SystemConfigRead>("/config/system", { signal });
}
