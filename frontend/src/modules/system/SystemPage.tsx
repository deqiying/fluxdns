import type { ReactNode } from "react";
import { Badge, Button, Descriptions, Flex, Space, Tag, Typography } from "antd";
import { RefreshCw } from "lucide-react";
import { ApiError } from "@/shared/api/errors";
import type { components } from "@/shared/api/generated-v2";
import { PageFrame } from "@/shared/components/PageFrame";
import { InlineUnavailable, PageState } from "@/shared/components/PageState";
import {
  formatBytesMiB,
  formatCount,
  formatDateTime,
  formatEpochMillis,
  formatPercent,
  formatUptimeClock,
} from "@/shared/formatters";
import { useDisplayedUptime, useProcessMetrics, useSystem } from "./hooks";

type Schemas = components["schemas"];
type Unavailable = Schemas["Unavailable"];

const capabilityLabels: Record<string, string> = {
  "read:overview": "总览",
  "read:runtime": "Runtime",
  "read:health": "健康状态",
  "read:statistics": "统计",
  "read:queries": "解析记录",
  "read:resources": "资源",
  "read:system": "系统",
};

function measurementValue<T extends number | string>(
  measurement: { state: "available"; value: T } | Unavailable,
  format: (value: T) => string,
): ReactNode {
  if (measurement.state === "unavailable") {
    return <InlineUnavailable reasonCode={measurement.reason} />;
  }
  return format(measurement.value);
}

function optionalSystemValue(value: ReactNode, loading: boolean, error: unknown): ReactNode {
  if (value) return value;
  if (loading) return <Typography.Text type="secondary">加载中</Typography.Text>;
  return <InlineUnavailable reasonCode={error instanceof ApiError ? error.code : undefined} />;
}

export function SystemPage() {
  const systemQuery = useSystem();
  const metricsQuery = useProcessMetrics();
  const system = systemQuery.data;
  const metrics = metricsQuery.data;
  const displayedUptime = useDisplayedUptime(metrics?.uptime_seconds, metricsQuery.dataUpdatedAt);
  const refreshing = metricsQuery.isFetching || systemQuery.isFetching;

  const refresh = () => {
    void Promise.all([metricsQuery.refetch(), systemQuery.refetch()]);
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
                {optionalSystemValue(system?.version || undefined, systemQuery.isLoading, systemQuery.error)}
              </Descriptions.Item>
              <Descriptions.Item label="启动时间">
                {optionalSystemValue(system?.started_at ? formatDateTime(system.started_at) : undefined, systemQuery.isLoading, systemQuery.error)}
              </Descriptions.Item>
              <Descriptions.Item label="采样时间">{formatEpochMillis(metrics.sampled_at_ms)}</Descriptions.Item>
              <Descriptions.Item label="管理能力" span={2}>
                {optionalSystemValue(
                  system ? (
                    system.capabilities.length ? (
                      <Space size={[6, 6]} wrap>
                        {system.capabilities.map((capability) => (
                          <Tag key={capability}>{capabilityLabels[capability] ?? capability}</Tag>
                        ))}
                      </Space>
                    ) : <Typography.Text type="secondary">未声明</Typography.Text>
                  ) : undefined,
                  systemQuery.isLoading,
                  systemQuery.error,
                )}
              </Descriptions.Item>
            </Descriptions>
          </section>
        </Space>
      ) : null}
    </PageFrame>
  );
}
