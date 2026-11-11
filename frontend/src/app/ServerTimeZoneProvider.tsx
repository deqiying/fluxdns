import type { ReactNode } from "react";
import { Alert, Button, Spin } from "antd";
import { useQuery } from "@tanstack/react-query";
import { processMetricsOptions } from "@/modules/system/api";
import { TimeZoneContext } from "@/shared/time/TimeZoneContext";
import { resolveTimeZone } from "@/shared/time/time-zone";

/** 仅在受保护壳层挂载，共用运行时查询缓存；先确定时区，再初始化页面日期筛选。 */
export function ServerTimeZoneProvider({ children }: { children: ReactNode }) {
  const query = useQuery(processMetricsOptions);
  // 首次失败也算读取完成；后续重试不能卸载已放行的页面并丢弃配置草稿。
  if (query.isPending && !query.isFetched) return <div role="status" aria-label="正在读取服务端时区"><Spin /> 正在读取服务端时区</div>;

  const resolved = resolveTimeZone(query.data?.timezone);
  const fallbackReason = query.data ? resolved.fallbackReason ?? query.data.timezone_fallback_reason : "timezone_request_failed";
  const value = { timeZone: fallbackReason ? "UTC" : resolved.timeZone, fallbackReason };
  return (
    <TimeZoneContext.Provider value={value}>
      {fallbackReason ? <Alert type="warning" showIcon title={`服务端时区不可用，当前使用 UTC（${fallbackReason}）`}
        action={<Button size="small" loading={query.isFetching} onClick={() => void query.refetch()}>重试时区读取</Button>} /> : null}
      {children}
    </TimeZoneContext.Provider>
  );
}
