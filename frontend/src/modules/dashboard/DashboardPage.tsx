import { Alert, Typography } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState, InlineUnavailable } from "@/shared/components/PageState";
import { formatBytesMiB, formatCount, formatEpochMillis } from "@/shared/formatters";
import type { ServiceMetrics } from "./api";
import { MetricsTrendChart } from "./MetricsTrendChart";
import { useServiceMetrics } from "./hooks";

type Measurement = ServiceMetrics["qps"] | ServiceMetrics["online_clients"] | ServiceMetrics["rss_bytes"];

export function DashboardPage() {
  const query = useServiceMetrics();
  const metrics = query.data;

  return (
    <PageFrame
      title="服务状态"
      description="主实例 · 最近十分钟"
      meta={metrics ? <Typography.Text type="secondary">采样：{formatEpochMillis(metrics.sampled_at_ms)}</Typography.Text> : undefined}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {metrics ? (
        <div className="service-status-content">
          {query.stale ? <Alert className="service-status-alert" type="warning" showIcon title="实时指标暂时不可用，当前显示最后一次有效快照" /> : null}
          <div className="service-status-metrics">
            <Metric label="当前内存" measurement={metrics.rss_bytes} formatter={(value) => formatBytesMiB(String(value))} />
            <Metric label="平均 QPS" measurement={metrics.qps} formatter={(value) => formatRate(Number(value))} />
            <Metric label="平均 RPM" measurement={metrics.rpm} formatter={(value) => formatRate(Number(value))} />
            <Metric label="在线客户端" measurement={metrics.online_clients} formatter={(value) => formatCount(Number(value))} />
          </div>
          <MetricsTrendChart metrics={metrics} />
        </div>
      ) : null}
    </PageFrame>
  );
}

function Metric({ label, measurement, formatter }: { label: string; measurement: Measurement; formatter: (value: string | number) => string }) {
  return (
    <div className="service-status-metric">
      <div className="metric-label">{label}</div>
      {measurement.state === "available"
        ? <div className="service-status-value">{formatter(measurement.value)}</div>
        : <InlineUnavailable reasonCode={unavailableLabel(measurement.reason, measurement.observed_seconds)} />}
    </div>
  );
}

function unavailableLabel(reason: string, observedSeconds: number | null): string {
  if (reason === "warmup") return `暖机中${observedSeconds === null ? "" : ` · ${observedSeconds}s`}`;
  if (reason === "observation_gap") return "观测存在缺口";
  if (reason === "sampling_failed") return "采样失败";
  return "平台不支持";
}

function formatRate(value: number): string {
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(value);
}
