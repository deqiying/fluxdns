import { queryOptions } from "@tanstack/react-query";
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

/** 时区入口和系统页共用有效期，避免入口读取后页面挂载立即重复请求。 */
export const processMetricsOptions = queryOptions({
  queryKey: processMetricsKey,
  queryFn: ({ signal }) => getProcessMetrics(signal),
  staleTime: 10_000,
  // 时区入口已尝试读取；系统页挂载不应使失败查询在两个入口之间循环重试。
  retryOnMount: false,
});

export function getServiceMetrics(signal?: AbortSignal): Promise<ServiceMetrics> {
  return apiV2Request<ServiceMetrics>("/service/metrics", { signal });
}

export function getSystemConfig(signal?: AbortSignal): Promise<SystemConfigRead> {
  return apiV2Request<SystemConfigRead>("/config/system", { signal });
}

export type MemoryAnalysis = Schemas["MemoryAnalysis"];
export const memoryAnalysisKey = ["api", "v2", "system", "memory"] as const;

/** GET 只读取 OS 样本与已存在的模块快照，不触发模块扫描。 */
export function getMemoryAnalysis(signal?: AbortSignal): Promise<MemoryAnalysis> {
  return apiV2Request<MemoryAnalysis>("/system/memory", { signal });
}

/** 手动采集只提交一次；超时或取消只结束前端等待，不保证服务端停止采集。 */
export function collectMemorySnapshot(signal: AbortSignal): Promise<MemoryAnalysis> {
  return apiV2Request<MemoryAnalysis>("/system/memory/snapshot", { method: "POST", signal });
}
