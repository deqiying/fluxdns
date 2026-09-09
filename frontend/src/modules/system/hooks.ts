import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { getSummaryPollInterval } from "@/app/query-client";
import { getProcessMetrics, processMetricsKey } from "./api";

export function useProcessMetrics() {
  return useQuery({
    queryKey: processMetricsKey,
    queryFn: ({ signal }) => getProcessMetrics(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
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
