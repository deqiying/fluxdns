import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { getSummaryPollInterval } from "@/app/query-client";
import {
  getProcessMetrics,
  getServiceMetrics,
  getSystemConfig,
  processMetricsKey,
  serviceMetricsKey,
  systemConfigKey,
} from "./api";

export function useProcessMetrics() {
  return useQuery({
    queryKey: processMetricsKey,
    queryFn: ({ signal }) => getProcessMetrics(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
  });
}

/** 在线身份计数沿用服务状态页同一份 /service/metrics 缓存，并按可见性 30 秒轮询。 */
export function useServiceMetrics() {
  return useQuery({
    queryKey: serviceMetricsKey,
    queryFn: ({ signal }) => getServiceMetrics(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
  });
}

/** 只读启动配置：工作路径和管理面入口随配置应用变化，进入页面时重新读取。 */
export function useSystemConfig() {
  return useQuery({
    queryKey: systemConfigKey,
    queryFn: ({ signal }) => getSystemConfig(signal),
  });
}

export function calculateDisplayedUptime(
  uptimeSeconds: number | undefined,
  receivedAtMs: number,
  nowMs: number,
): number | undefined {
  if (!Number.isSafeInteger(uptimeSeconds) || uptimeSeconds === undefined || uptimeSeconds < 0) return undefined;
  if (!Number.isSafeInteger(receivedAtMs) || receivedAtMs <= 0 || !Number.isSafeInteger(nowMs)) return undefined;
  const elapsedSeconds = Math.max(0, Math.floor((nowMs - receivedAtMs) / 1_000));
  const result = uptimeSeconds + elapsedSeconds;
  return Number.isSafeInteger(result) ? result : undefined;
}

/** 页面隐藏时停止逐秒渲染；重新可见后按响应接收时刻校正，不伪造新的服务端样本。 */
export function useDisplayedUptime(uptimeSeconds: number | undefined, receivedAtMs: number): number | undefined {
  const [nowMs, setNowMs] = useState(() => Date.now());

  useEffect(() => {
    let timer: number | undefined;
    const syncTimer = () => {
      if (timer !== undefined) window.clearInterval(timer);
      setNowMs(Date.now());
      timer = document.visibilityState === "visible"
        ? window.setInterval(() => setNowMs(Date.now()), 1_000)
        : undefined;
    };

    document.addEventListener("visibilitychange", syncTimer);
    syncTimer();
    return () => {
      document.removeEventListener("visibilitychange", syncTimer);
      if (timer !== undefined) window.clearInterval(timer);
    };
  }, [receivedAtMs, uptimeSeconds]);

  return calculateDisplayedUptime(uptimeSeconds, receivedAtMs, nowMs);
}
