import type { ReactNode } from "react";
import { Badge, Button, Descriptions, Flex, Space, Typography } from "antd";
import { RefreshCw } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { PageFrame } from "@/shared/components/PageFrame";
import { InlineUnavailable, PageState } from "@/shared/components/PageState";
import {
  formatBytesMiB,
  formatCount,
  formatEpochMillis,
  formatPercent,
  formatUptimeClock,
} from "@/shared/formatters";
import { useDisplayedUptime, useProcessMetrics } from "./hooks";

type Schemas = components["schemas"];
type Unavailable = Schemas["Unavailable"];

function measurementValue<T extends number | string>(
  measurement: { state: "available"; value: T } | Unavailable,
  format: (value: T) => string,
): ReactNode {
  if (measurement.state === "unavailable") {
    return <InlineUnavailable reasonCode={measurement.reason} />;
  }
  return format(measurement.value);
}

export function SystemPage() {
  const metricsQuery = useProcessMetrics();
  const metrics = metricsQuery.data;
  const displayedUptime = useDisplayedUptime(metrics?.uptime_seconds, metricsQuery.dataUpdatedAt);
  const refreshing = metricsQuery.isFetching;

  const refresh = () => {
    void metricsQuery.refetch();
  };

  return (
    <PageFrame
      title="系统运行状态"
      description="主实例的进程与基础运行信息。"
      meta={metrics ? (
        <Typography.Text type="secondary">采样：{formatEpochMillis(metrics.sampled_at_ms)}</Typography.Text>
      ) : undefined}
      actions={(
        <Button icon={<RefreshCw size={16} />} loading={refreshing} onClick={refresh}>
          刷新
        </Button>
      )}
    >
      <PageState
        loading={metricsQuery.isLoading}
        error={metrics ? undefined : metricsQuery.error}
        onRetry={() => void metricsQuery.refetch()}
      />
      {metrics ? (
        <Space className="system-runtime-content" orientation="vertical" size={30}>
          <div className="system-runtime-metrics" role="list" aria-label="进程指标">
            <div className="system-runtime-metric" role="listitem">
              <Typography.Text type="secondary">运行时长</Typography.Text>
              <div className="system-runtime-value">
                {displayedUptime === undefined
                  ? <InlineUnavailable reasonCode="invalid_sample" />
                  : formatUptimeClock(displayedUptime)}
              </div>
            </div>
            <div className="system-runtime-metric" role="listitem">
              <Typography.Text type="secondary">常驻内存</Typography.Text>
              <div className="system-runtime-value">{measurementValue(metrics.rss_bytes, formatBytesMiB)}</div>
            </div>
            <div className="system-runtime-metric" role="listitem">
              <Typography.Text type="secondary">CPU</Typography.Text>
              <div className="system-runtime-value">{measurementValue(metrics.cpu_percent, formatPercent)}</div>
            </div>
            <div className="system-runtime-metric" role="listitem">
              <Typography.Text type="secondary">线程数</Typography.Text>
              <div className="system-runtime-value">{measurementValue(metrics.threads, formatCount)}</div>
            </div>
          </div>

          <section className="system-runtime-details" aria-labelledby="process-information-title">
            <Flex justify="space-between" align="center" gap={16} wrap>
              <Typography.Title id="process-information-title" level={4}>进程信息</Typography.Title>
              <Badge status="success" text="运行中" />
            </Flex>
            <Descriptions className="system-runtime-descriptions" column={{ xs: 1, md: 2 }} colon={false}>
              <Descriptions.Item label="程序">FluxDNS</Descriptions.Item>
              <Descriptions.Item label="版本">
                {metrics.version}
              </Descriptions.Item>
              <Descriptions.Item label="启动时间">
                {formatEpochMillis(metrics.started_at_ms)}
              </Descriptions.Item>
              <Descriptions.Item label="采样时间">{formatEpochMillis(metrics.sampled_at_ms)}</Descriptions.Item>
            </Descriptions>
          </section>
        </Space>
      ) : null}
    </PageFrame>
  );
}
