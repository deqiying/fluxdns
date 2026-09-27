import type { ReactNode } from "react";
import { Badge, Button, Flex, Typography } from "antd";
import { RefreshCw } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ApiError } from "@/shared/api/errors";
import { ConfigSyncBadge } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { InlineUnavailable, PageState } from "@/shared/components/PageState";
import { useConfigModule, useConfigState } from "@/shared/config/hooks";
import {
  formatBytes,
  formatCount,
  formatEpochMillis,
  formatPercent,
  formatUptime,
  formatUptimeClock,
} from "@/shared/formatters";
import { useDisplayedUptime, useProcessMetrics, useServiceMetrics, useSystemConfig } from "./hooks";

type Schemas = components["schemas"];
type Unavailable = Schemas["Unavailable"];
type ModuleRuntime = Schemas["ModuleRuntime"];
type ListenerBinding = Extract<ModuleRuntime, { module: "listener" }>["bindings"][number];

/** 传输类型按协议习惯写法呈现，不直接把枚举小写值渲染到页面。 */
const transportLabels: Record<Schemas["Transport"], string> = { udp: "UDP", tcp: "TCP", doh: "DoH" };

const cacheStateLabels: Record<Schemas["CacheSnapshotStatus"]["state"], string> = {
  disabled: "已禁用",
  idle: "空闲",
  writing: "写入中",
  restoring: "恢复中",
  failed: "失败",
};

/** 进程指标的 4 个同构格子：标签、数值、口径说明固定三段，跨列基线对齐。 */
function MetricCell({ label, value, note }: { label: string; value: ReactNode; note: string }) {
  return (
    <div className="system-metric-cell" role="listitem">
      <span className="system-metric-label">{label}</span>
      <div className="system-metric-value">{value}</div>
      <span className="system-metric-note">{note}</span>
    </div>
  );
}

/** 运行信息行与上方指标格共用同一条列分界，时间与版本不再另起一行表头。 */
function InfoCell({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="system-info-cell" role="group" aria-label={label}>
      <span className="system-info-label">{label}</span>
      <span className="system-info-value">{value}</span>
    </div>
  );
}

function DetailRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="system-detail-row" role="group" aria-label={label}>
      <span className="system-detail-label">{label}</span>
      <span className="system-detail-value">{children}</span>
    </div>
  );
}

function measurementValue<T extends number | string>(
  measurement: { state: "available"; value: T } | Unavailable,
  format: (value: T) => string,
): ReactNode {
  if (measurement.state === "unavailable") {
    return <InlineUnavailable reasonCode={measurement.reason} />;
  }
  return format(measurement.value);
}

/**
 * 次级卡片读取失败时保留契约错误码；加载中返回 null，由调用方用占位符区分，
 * 不把「还没返回」渲染成「不可用」。
 */
function failureValue(isError: boolean, error: unknown): ReactNode {
  return isError ? <InlineUnavailable reasonCode={error instanceof ApiError ? error.code : undefined} /> : null;
}

function hostText(value: string | null | undefined): ReactNode {
  return value ? value : <InlineUnavailable />;
}

function bindingLabel(binding: ListenerBinding): string {
  const transports = transportLabels[binding.transport];
  const accepting = binding.accepting ? "" : "（暂停接受）";
  return `${transports} ${binding.address}:${binding.port}${accepting}`;
}

/** 资源就绪概览：没有配置该模块时返回 null，避免用 0/0 冒充正常状态。 */
function readySummary(runtime: ModuleRuntime[] | undefined): string | null {
  const conditions = (runtime ?? []).flatMap((item) => ("condition" in item ? [item.condition] : []));
  if (conditions.length === 0) return null;
  const ready = conditions.filter((condition) => condition === "ready").length;
  return `${ready}/${conditions.length} 就绪`;
}

export function SystemPage() {
  const metricsQuery = useProcessMetrics();
  const metrics = metricsQuery.data;
  const displayedUptime = useDisplayedUptime(metrics?.uptime_seconds, metricsQuery.dataUpdatedAt);
  const systemQuery = useSystemConfig();
  const stateQuery = useConfigState();
  const serviceQuery = useServiceMetrics();
  const listenerQuery = useConfigModule("listener");
  const dnsQuery = useConfigModule("dns");
  const hostsQuery = useConfigModule("hosts");
  const ruleSetsQuery = useConfigModule("rule_set");

  const queries = [metricsQuery, systemQuery, stateQuery, serviceQuery, listenerQuery, dnsQuery, hostsQuery, ruleSetsQuery];
  const refreshing = queries.some((query) => query.isFetching);
  const refresh = () => {
    void Promise.all(queries.map((query) => query.refetch()));
  };

  const host = metrics?.host;
  const bindings: ListenerBinding[] = listenerQuery.data?.runtime.flatMap(
    (runtime) => (runtime.module === "listener" ? runtime.bindings : []),
  ) ?? [];
  const cacheSnapshot = dnsQuery.data?.runtime.flatMap(
    (runtime) => (runtime.module === "dns" ? [runtime.snapshot] : []),
  )[0];
  const state = stateQuery.data;
  const hostsSummary = readySummary(hostsQuery.data?.runtime);
  const ruleSetSummary = readySummary(ruleSetsQuery.data?.runtime);

  const listenerValue = failureValue(listenerQuery.isError, listenerQuery.error) ?? (listenerQuery.data === undefined
    ? "—"
    : bindings.length === 0
      ? "无监听绑定"
      : `${bindings.length} 个绑定 · ${bindings.slice(0, 3).map(bindingLabel).join(" · ")}${
          bindings.length > 3 ? ` · 另 ${bindings.length - 3} 个` : ""
        }`);

  const cacheValue = failureValue(dnsQuery.isError, dnsQuery.error) ?? (cacheSnapshot === undefined
    ? "—"
    : `${cacheStateLabels[cacheSnapshot.state]} · 第 ${cacheSnapshot.generation} 代${
        cacheSnapshot.file_bytes === null ? "" : ` · ${formatBytes(cacheSnapshot.file_bytes)}`
      }`);

  const revisionValue = failureValue(stateQuery.isError, stateQuery.error) ?? (state === undefined ? "—" : (
    <Flex align="center" gap={8} wrap>
      <Typography.Text code>{state.active_revision}</Typography.Text>
      <ConfigSyncBadge state={state} />
    </Flex>
  ));

  const resourceValue = failureValue(hostsQuery.isError || ruleSetsQuery.isError, hostsQuery.error ?? ruleSetsQuery.error)
    ?? ([hostsSummary ? `Hosts ${hostsSummary}` : null, ruleSetSummary ? `规则集 ${ruleSetSummary}` : null]
      .filter((item): item is string => item !== null)
      .join(" · ") || "—");

  const onlineClientsValue = failureValue(serviceQuery.isError, serviceQuery.error) ?? (serviceQuery.data === undefined
    ? "—"
    : measurementValue(serviceQuery.data.online_clients, (value) => `${formatCount(value)} 个`));

  const workPathValue = failureValue(systemQuery.isError, systemQuery.error)
    ?? (systemQuery.data === undefined ? "—" : <Typography.Text code>{systemQuery.data.work_path}</Typography.Text>);

  return (
    <PageFrame
      title="系统运行状态"
      description="每一次采样，如实呈现。"
      actions={(
        <Flex align="center" gap={12}>
          <Typography.Text type="secondary">每 30 秒自动刷新</Typography.Text>
          <Button icon={<RefreshCw size={16} />} loading={refreshing} onClick={refresh}>
            刷新
          </Button>
        </Flex>
      )}
    >
      <PageState
        loading={metricsQuery.isLoading}
        error={metrics ? undefined : metricsQuery.error}
        onRetry={() => void refresh()}
      />
      {metrics ? (
        <div className="system-runtime-sections">
          <section className="system-runtime-card" aria-labelledby="system-runtime-metrics-title">
            <div className="system-runtime-card-head">
              <Flex justify="space-between" align="center" gap={16} wrap>
                <Typography.Title id="system-runtime-metrics-title" level={4}>进程指标</Typography.Title>
                <Badge status="success" text="运行中" />
              </Flex>
            </div>
            <div className="system-metric-grid" role="list" aria-label="进程指标">
              <MetricCell
                label="运行时长"
                value={displayedUptime === undefined
                  ? <InlineUnavailable reasonCode="invalid_sample" />
                  : formatUptimeClock(displayedUptime)}
                note={displayedUptime === undefined ? "自启动累计时间暂不可用" : `自启动累计 ${formatUptime(displayedUptime)}`}
              />
              <MetricCell
                label="常驻内存"
                value={measurementValue(metrics.rss_bytes, formatBytes)}
                note="进程 RSS，含映射页"
              />
              <MetricCell
                label="CPU"
                value={measurementValue(metrics.cpu_percent, formatPercent)}
                note="相对单核，1 秒采样窗口"
              />
              <MetricCell
                label="线程数"
                value={measurementValue(metrics.threads, formatCount)}
                note="含运行时工作线程"
              />
            </div>
            <div className="system-info-row">
              <InfoCell label="版本" value={metrics.version} />
              <InfoCell label="启动时间" value={formatEpochMillis(metrics.started_at_ms)} />
              <InfoCell label="采样时间" value={formatEpochMillis(metrics.sampled_at_ms)} />
              <InfoCell label="采样来源" value="主实例进程" />
            </div>
          </section>

          <div className="system-detail-grid">
            <section className="system-detail-card" aria-labelledby="system-runtime-host-title">
              <div className="system-detail-card-head">
                <Typography.Title id="system-runtime-host-title" level={4}>运行环境</Typography.Title>
              </div>
              <div className="system-detail-rows">
                <DetailRow label="操作系统">{hostText(host?.os)}</DetailRow>
                <DetailRow label="内核 / 构建版本">{hostText(host?.kernel)}</DetailRow>
                <DetailRow label="CPU 架构">
                  {host ? `${host.arch} · ${formatCount(host.logical_cores)} 逻辑核心` : "—"}
                </DetailRow>
                <DetailRow label="主机名">{hostText(host?.hostname)}</DetailRow>
                <DetailRow label="进程 ID">{host ? formatCount(host.process_id) : "—"}</DetailRow>
                <DetailRow label="数据目录">{workPathValue}</DetailRow>
              </div>
            </section>

            <section className="system-detail-card" aria-labelledby="system-runtime-dataplane-title">
              <div className="system-detail-card-head">
                <Typography.Title id="system-runtime-dataplane-title" level={4}>数据面摘要</Typography.Title>
              </div>
              <div className="system-detail-rows">
                <DetailRow label="监听入口">{listenerValue}</DetailRow>
                <DetailRow label="在线客户端">{onlineClientsValue}</DetailRow>
                <DetailRow label="DNS 缓存快照">{cacheValue}</DetailRow>
                <DetailRow label="配置版本">{revisionValue}</DetailRow>
                <DetailRow label="资源状态">{resourceValue}</DetailRow>
              </div>
            </section>
          </div>
        </div>
      ) : null}
    </PageFrame>
  );
}
