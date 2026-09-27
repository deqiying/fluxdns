import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { managementEvents, type EventConnectionState } from "@/shared/api/events";
import { getServiceMetrics, serviceMetricsKey } from "./api";

/** 后端每秒推送一次指标；连续三个推送周期没有新数据才视为延迟。 */
export const METRICS_STALE_AFTER_MS = 3_000;

/**
 * 活跃度只以本地收到指标的时刻为基准。
 * 服务端 `sampled_at_ms` 来自后端主机时钟，与浏览器时钟可能相差数秒（例如后端跑在
 * 另一台机器、容器或时钟未同步的主机上）；用两者差值判断会在每个推送周期内
 * 反复切换"实时连接正常/指标更新延迟"，形成每秒一次的闪烁。
 */
export function isMetricsStale(input: {
  hasData: boolean;
  dataUpdatedAt: number;
  nowMs: number;
  connectionState: EventConnectionState;
}): boolean {
  if (!input.hasData) return false;
  if (input.connectionState === "error") return true;
  return input.nowMs - input.dataUpdatedAt > METRICS_STALE_AFTER_MS;
}

export function useServiceMetrics() {
  const queryClient = useQueryClient();
  const [connectionState, setConnectionState] = useState<EventConnectionState>("connecting");
  const [, setClockTick] = useState(0);
  const query = useQuery({
    queryKey: serviceMetricsKey,
    queryFn: ({ signal }) => getServiceMetrics(signal),
    staleTime: 3_000,
    refetchOnWindowFocus: false,
  });

  useEffect(() => {
    if (!query.isSuccess) return;
    let active = true;
    let unsubscribe: (() => void) | undefined;
    const subscribe = () => {
      if (!active || document.visibilityState === "hidden") return;
      unsubscribe = managementEvents.subscribeMetrics(
        (metrics) => queryClient.setQueryData(serviceMetricsKey, metrics),
        setConnectionState,
      );
    };
    const visibilityChanged = () => {
      unsubscribe?.();
      unsubscribe = undefined;
      if (document.visibilityState !== "hidden") {
        void query.refetch().then(() => subscribe());
      }
    };
    subscribe();
    document.addEventListener("visibilitychange", visibilityChanged);
    return () => {
      active = false;
      document.removeEventListener("visibilitychange", visibilityChanged);
      unsubscribe?.();
    };
  }, [query.isSuccess, query.refetch, queryClient]);

  // 推送停止后没有事件驱动重渲染，用每秒一次的空更新重新判定延迟。
  useEffect(() => {
    const timer = window.setInterval(() => setClockTick((tick) => tick + 1), 1_000);
    return () => window.clearInterval(timer);
  }, []);

  const stale = isMetricsStale({
    hasData: query.data !== undefined,
    dataUpdatedAt: query.dataUpdatedAt,
    nowMs: Date.now(),
    connectionState,
  });
  return { ...query, connectionState, stale };
}
