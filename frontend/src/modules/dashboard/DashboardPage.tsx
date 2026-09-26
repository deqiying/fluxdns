import { useState } from "react";
import { Alert, Button, Tooltip } from "antd";
import { Activity, Cpu, Moon, Sun, Users, type LucideIcon } from "lucide-react";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState, InlineUnavailable } from "@/shared/components/PageState";
import { formatBytes, formatCount, formatEpochMillis } from "@/shared/formatters";
import type { ServiceMetrics } from "./api";
import { MetricsTrendChart } from "./MetricsTrendChart";
import { useServiceMetrics } from "./hooks";
import { useRateTrend } from "./rateTrend";

type Measurement = ServiceMetrics["qps"] | ServiceMetrics["online_clients"] | ServiceMetrics["rss_bytes"];

/** 状态标识只描述指标连接，不将快照存在等同于服务健康。 */
export function DashboardPage() {
  // 深色样例只属于当前服务状态页，不持久化偏好或改变其他路由的主题。
  const [darkPreview, setDarkPreview] = useState(false);
  const query = useServiceMetrics();
  const metrics = query.data;
  const rateTrend = useRateTrend(metrics);
  const live = !!metrics && !query.error && !query.stale && query.connectionState === "open";
  const disconnected = !!query.error || query.connectionState === "error" || query.connectionState === "closed";
  const status = disconnected ? "实时连接中断"
    : query.stale ? "指标更新延迟"
      : live ? "实时连接正常"
        : query.connectionState === "reconnecting" ? "正在重新连接" : "正在连接实时指标";
  const themeLabel = darkPreview ? "浅色显示" : "深色样例";

  return (
    <div className={darkPreview ? "service-status-preview service-status-preview-dark" : "service-status-preview"}>
      <PageFrame
        title="服务状态"
        description="每一次解析，尽在掌握。"
        actions={
          <div className="service-status-heading-meta">
            <div className="service-status-actions">
              <span role="status" className={`service-connection service-connection-${live ? "live" : disconnected ? "error" : "pending"}`}>
                <i aria-hidden="true" />{status}
              </span>
              <Tooltip title={themeLabel}>
                <Button className="service-theme-toggle" aria-label={themeLabel} aria-pressed={darkPreview}
                  icon={darkPreview ? <Sun size={17} aria-hidden="true" /> : <Moon size={17} aria-hidden="true" />}
                  onClick={() => setDarkPreview((value) => !value)} />
              </Tooltip>
            </div>
            <span className="service-status-sampled-at">{metrics ? `最后采样 ${formatEpochMillis(metrics.sampled_at_ms)}` : "等待首次采样"}</span>
          </div>
        }
      >
        <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
        {metrics ? (
          <div className="service-status-content">
            {query.stale ? <Alert className="service-status-alert" type="warning" showIcon title="实时指标暂时不可用，当前显示最后一次有效快照" /> : null}
            <div className="service-status-metrics">
              <Metric label="当前内存" measurement={metrics.rss_bytes} formatter={(value) => formatBytes(String(value))} hint="进程驻留内存" icon={Cpu} />
              <Metric label="平均 QPS" measurement={metrics.qps} formatter={(value) => formatRate(Number(value))} unit="请求/秒" hint="每秒请求速率" icon={Activity} tone="qps" />
              <Metric label="平均 RPM" measurement={metrics.rpm} formatter={(value) => formatRate(Number(value))} unit="请求/分钟" hint="每分钟请求速率" icon={Activity} tone="rpm" />
              <Metric label="在线客户端" measurement={metrics.online_clients} formatter={(value) => formatCount(Number(value))} unit="个" hint="当前活跃身份" icon={Users} />
            </div>
            <MetricsTrendChart metrics={metrics} rateTrend={rateTrend} />
            <div className="service-status-footer"><span>主实例</span><span>最近 10 分钟 · UTC</span></div>
          </div>
        ) : null}
      </PageFrame>
    </div>
  );
}

/** 指标不可用时保留原因，不显示零值或孤立的单位。 */
function Metric({ label, measurement, formatter, unit, hint, icon: Icon, tone = "neutral" }: {
  label: string;
  measurement: Measurement;
  formatter: (value: string | number) => string;
  unit?: string;
  hint: string;
  icon: LucideIcon;
  tone?: "neutral" | "qps" | "rpm";
}) {
  return (
    <div className="service-status-metric" role="group" aria-label={label}>
      <div className="service-status-metric-heading"><span className="metric-label">{label}</span><Icon className={`metric-icon metric-icon-${tone}`} size={21} strokeWidth={1.7} aria-hidden="true" /></div>
      {measurement.state === "available"
        ? <div className="service-status-value"><span>{formatter(measurement.value)}</span>{unit ? <>{" "}<span className="service-status-unit">{unit}</span></> : null}</div>
        : <div className="service-status-unavailable"><InlineUnavailable reasonCode={unavailableLabel(measurement.reason, measurement.observed_seconds)} /></div>}
      <div className="service-status-hint">{hint}</div>
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
