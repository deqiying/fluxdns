import { useId, useRef, useState, type KeyboardEvent } from "react";
import { Button, Typography } from "antd";
import { Info, RefreshCw } from "lucide-react";
import { getSafeErrorMessage, safeText } from "@/shared/api/errors";
import { formatEpochMillis } from "@/shared/formatters";
import { useServerTimeZone } from "@/shared/time/TimeZoneContext";
import type { MemoryAnalysis } from "./api";
import { useMemoryAnalysis } from "./memory-hooks";
import { counterLabels, formatMemoryBytes, measuredBytes, moduleDelta, moduleLabels, parseMemoryBytes, rssComposition, type BytesMeasurement, type MemoryModule, type MemoryOsSample } from "./memory-model";
import { MemoryTrend } from "./MemoryTrend";

const tabs = ["模块明细", "变化趋势", "采样口径"];
const partLabels = ["匿名内存", "文件映射", "共享内存"];
const colors = ["#007aff", "#19a6a0", "#b8c1cd"];

function measurementText(value: BytesMeasurement): string {
  if (value.state === "unavailable") return `${value.reason === "unsupported" ? "不支持" : "暂不可用"}（${value.reason}）`;
  const bytes = measuredBytes(value);
  return bytes === null ? "暂不可用（invalid_sample）" : formatMemoryBytes(bytes);
}

function OsSummary({ os }: { os: MemoryOsSample }) {
  const composition = rssComposition(os);
  const parts = [os.anonymous_bytes, os.file_bytes, os.shared_bytes];
  const total = composition.values.reduce((sum, bytes) => sum + bytes, 0n);
  let offset = 0;
  return (
    <div className="memory-summary">
      <div className="memory-rss-summary">
        <div className="memory-eyebrow">进程常驻内存 <span className="memory-badge">OS 实测</span></div>
        <div className="memory-rss-value">{measurementText(os.rss_bytes)}</div>
        <div className="memory-submetrics"><span>峰值 <b>{measurementText(os.peak_rss_bytes)}</b></span><span>PSS <b>{measurementText(os.pss_bytes)}</b></span></div>
      </div>
      <div className="memory-allocation">
        <div className="memory-allocation-heading"><span>RSS 构成</span><small>同轮非原子采样 · MiB = 1024² bytes</small></div>
        {composition.reason ? <p className="memory-composition-note">{composition.reason}</p> : (
          <svg className="memory-allocation-svg" viewBox="0 0 1000 20" preserveAspectRatio="none" role="img" aria-label={`RSS 构成：${parts.map((part, index) => `${partLabels[index]} ${measurementText(part)}`).join("，")}`}>
            <title>RSS 构成</title>
            {composition.values.map((bytes, index) => {
              const width = Number(bytes * 1_000_000n / total) / 1_000;
              const x = offset;
              offset += width;
              return <rect key={partLabels[index]} x={x} width={width} height="20" fill={colors[index]} />;
            })}
          </svg>
        )}
        <div className="memory-allocation-legend">{parts.map((part, index) => <div key={partLabels[index]}><span><i style={{ background: colors[index] }} />{partLabels[index]}</span><strong>{measurementText(part)}</strong></div>)}</div>
      </div>
    </div>
  );
}

function ModuleCounters({ module }: { module: MemoryModule }) {
  return <div className="memory-counters">{module.counters.map((counter) => {
    const value = parseMemoryBytes(counter.value);
    const kind = counter.kind === "billing_bytes" ? "计费字节，非内存" : counter.kind === "budget_bytes" ? "预算，非实占" : "计数";
    const label = Object.hasOwn(counterLabels, counter.key) ? counterLabels[counter.key] : safeText(counter.key, 128) ?? "未知计数";
    return <span key={counter.key}>{label}：<b>{value === null ? "暂不可用" : counter.kind === "count" ? value.toLocaleString("zh-CN") : formatMemoryBytes(value)}</b><small>（{kind}）</small></span>;
  })}</div>;
}

function RulesDetails({ module }: { module: MemoryModule }) {
  return <details className="memory-rules"><summary>展开 {module.rules.length} 个唯一索引</summary><div className="memory-rule-list">
    {module.rules.map((rule, index) => <div className="memory-rule" key={`${rule.resource}-${rule.selector}-${index}`}>
      <code>{safeText(rule.resource, 256)}{rule.selector ? ` / ${safeText(rule.selector, 256)}` : ""}</code>
      <span>{rule.rule_count} 条 · {formatMemoryBytes(parseMemoryBytes(rule.estimated_bytes))}</span>
      <span>持有者：{rule.holders.map((holder) => ({ initial: "初始快照", worker: "worker", policy: "Policy" })[holder]).join(" / ") || "—"}{rule.retained ? " · 保留旧索引" : " · 当前索引"}</span>
    </div>)}
    <p>共享索引按唯一身份去重；仅已知载荷估算，不代表完整堆内存。</p>
  </div></details>;
}

function ModuleTable({ data }: { data: MemoryAnalysis }) {
  const { timeZone } = useServerTimeZone();
  if (!data.snapshot) return <div className="memory-empty"><h3>尚未采集模块快照</h3><p>OS 样本会自动更新；模块扫描仅在点击「采集分析快照」后执行。</p></div>;
  const modules = Object.keys(moduleLabels) as MemoryModule["id"][];
  return <>
    <div className="memory-module-intro">快照 #{data.snapshot.snapshot_id} · {formatEpochMillis(data.snapshot.sampled_at_ms, timeZone)} · runtime revision {data.snapshot.runtime_revision}</div>
    <table className="memory-module-table"><thead><tr><th scope="col">内存来源</th><th scope="col">已知载荷估算</th><th scope="col">较上次</th><th scope="col">采集口径 / 运行时信息</th></tr></thead><tbody>
      {modules.map((id) => {
        const module = data.snapshot!.modules.find((item) => item.id === id);
        const delta = module ? moduleDelta(data, module) : null;
        const bytes = module?.state === "available" ? parseMemoryBytes(module.estimated_bytes) : null;
        return <tr key={id}><th scope="row"><span>{moduleLabels[id]}</span>{id === "rules" && module && module.rules.length > 0 ? <RulesDetails module={module} /> : null}</th>
          <td data-label="已知载荷估算" className="memory-module-value">{bytes !== null ? formatMemoryBytes(bytes) : module?.state === "busy" ? "忙碌，暂不可用" : "未采集"}</td>
          <td data-label="较上次" className="memory-module-delta">{delta === null ? "不可比" : `${delta > 0n ? "+" : ""}${formatMemoryBytes(delta)}`}</td>
          <td data-label="采集口径" className="memory-module-basis"><span className="memory-badge">{bytes !== null ? "已知载荷 · 非完整内存" : module?.state === "available" ? "仅运行时计数" : module?.state === "busy" ? "采集忙碌" : "未接入字节探针"}</span>
            {module ? <ModuleCounters module={module} /> : null}
            {id === "dns_cache" ? <p>weight 是淘汰计费，不是真实内存。</p> : id === "sqlite" ? <p>租约预算不等于实占；数据库文件大小不是内存。</p> : id === "allocator" ? <p>未归因空间不等于泄漏。</p> : null}
          </td></tr>;
      })}
    </tbody></table>
  </>;
}

/** OS 与应用估算分层展示；父页面的进程身份用于阻止重启后的旧缓存展示与比较。 */
export function MemoryAnalysisCard({ processId, startedAtMs }: { processId: number; startedAtMs: number }) {
  const { query, pending, collect, cancel, collectionError } = useMemoryAnalysis();
  const { timeZone } = useServerTimeZone();
  const [tab, setTab] = useState(0);
  const tabRefs = useRef<(HTMLButtonElement | null)[]>([]);
  const id = useId();
  const response = query.data;
  const processChanged = response && (response.process_id !== processId || response.started_at_ms !== startedAtMs);
  const data = processChanged ? undefined : response;
  const unavailable = data ? Object.values(data.os).filter((value): value is Extract<BytesMeasurement, { state: "unavailable" }> => typeof value === "object" && value !== null && value.state === "unavailable") : [];
  const unsupported = unavailable.length === 8 && unavailable.every((value) => value.reason === "unsupported");
  const failed = !!collectionError || query.isError || data?.collection_state === "failed";
  const partial = unavailable.length > 0 || (data ? rssComposition(data.os).reason !== null : false)
    || (data?.snapshot ? Object.keys(moduleLabels).some((key) => !data.snapshot!.modules.some((module) => module.id === key && module.state === "available")) : false);
  const status = failed ? "失败" : unsupported ? "不支持" : partial ? "部分不可用" : "正常";
  const onTabKey = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const next = event.key === "ArrowRight" ? (index + 1) % tabs.length : event.key === "ArrowLeft" ? (index + tabs.length - 1) % tabs.length : event.key === "Home" ? 0 : event.key === "End" ? tabs.length - 1 : null;
    if (next !== null) { event.preventDefault(); setTab(next); tabRefs.current[next]?.focus(); }
  };

  return <section className="system-runtime-card memory-card" aria-labelledby={`${id}-title`}>
    <div className="system-runtime-card-head memory-head"><div className="memory-heading"><Typography.Title id={`${id}-title`} level={4}>内存占用分析</Typography.Title>{data ? <span className="memory-status" data-state={status}>{status}</span> : null}</div>
      <div className="memory-head-actions">{pending ? <Button onClick={cancel}>取消等待</Button> : null}<Button icon={<RefreshCw size={14} />} disabled={pending || !data || data.collection_state === "collecting"} onClick={() => void collect()}>{pending || data?.collection_state === "collecting" ? "采集中…" : "采集分析快照"}</Button></div>
    </div>
    {query.isError ? <div role="alert" className="memory-notice">内存分析读取失败：{getSafeErrorMessage(query.error)}{data ? "；下方保留上次读取结果，非本次采样。" : ""}<Button size="small" onClick={() => void query.refetch()}>重试读取</Button></div> : null}
    {collectionError || data?.collection_state === "failed" ? <div role="alert" className="memory-notice">本次采集未成功：{collectionError ? getSafeErrorMessage(collectionError) : data?.last_failure}。{data?.snapshot ? "下方为历史成功快照，不代表本次采集成功。" : "不会用 0 填补缺失读数。"}</div> : null}
    {pending || data?.collection_state === "collecting" ? <p role="status" className="memory-notice">正在等待采集；已有快照仍为历史结果。取消等待不保证服务端采集停止。</p> : null}
    {data ? <>
      <OsSummary os={data.os} />
      <div className="memory-insight"><Info size={15} aria-hidden="true" /><span>匿名内存也可能包含线程栈与分配器保留空间，不能直接等同于规则集占用。</span></div>
      <div className="memory-tabbar"><div role="tablist" aria-label="内存分析视图">{tabs.map((label, index) => <button key={label} type="button" role="tab" id={`${id}-tab-${index}`} aria-selected={tab === index} aria-controls={`${id}-panel-${index}`} tabIndex={tab === index ? 0 : -1} ref={(node) => { tabRefs.current[index] = node; }} onKeyDown={(event) => onTabKey(event, index)} onClick={() => setTab(index)}>{label}</button>)}</div><span>OS 实测与应用估算分层展示</span></div>
      <div role="tabpanel" id={`${id}-panel-0`} aria-labelledby={`${id}-tab-0`} hidden={tab !== 0}><ModuleTable data={data} /></div>
      <div role="tabpanel" id={`${id}-panel-1`} aria-labelledby={`${id}-tab-1`} hidden={tab !== 1}>{tab === 1 ? <MemoryTrend data={data} /> : null}</div>
      <div role="tabpanel" id={`${id}-panel-2`} aria-labelledby={`${id}-tab-2`} hidden={tab !== 2}><div className="memory-method-grid">
        <div><h3>OS 实测：进程占用了什么页</h3><p>OS 每 {data.sample_interval_seconds} 秒采样。Linux 的 status 与 smaps_rollup 分别读取，同一轮不是原子快照；不支持的指标保留原因，不补零。</p><p>Private Dirty：{measurementText(data.os.private_dirty_bytes)} · Swap：{measurementText(data.os.swap_bytes)}</p></div>
        <div><h3>应用估算：哪些对象仍被持有</h3><p>模块仅手动只读采集，不触发 GC、清空缓存、规则重载或内存回收。规则只估算已知载荷，共享索引去重，不代表完整堆占用。</p><p>只有同进程、相同 runtime revision 与估算版本、模块可用时才比较；不跨进程引用旧快照。</p></div>
        <div><h3>规则估算的覆盖边界</h3><p>rule-payload-v1 只统计已知对象、键、字符串与正则载荷容量；不含 BTree 内部节点与空槽、Arc 控制块、分配器元数据或解析工作区。</p><p>初始快照独占可达的索引标记为保留旧索引，不代表已证明进程中的全部引用。</p></div>
      </div></div>
      <footer className="memory-footer"><span>未归因不等于泄漏。模块估算不覆盖全部进程内存，不与 RSS 合计或计算差额。</span><span>OS：{formatEpochMillis(data.os.sampled_at_ms, timeZone)}{data.previous_snapshot ? ` · 上次快照 #${data.previous_snapshot.snapshot_id}` : ""}</span></footer>
    </> : <div className="memory-empty" role="status">{processChanged ? "进程已变化，等待新进程的内存样本；旧快照不参与比较。" : query.isLoading ? "正在读取内存样本…" : "内存样本暂不可用。"}{processChanged ? <Button onClick={() => void query.refetch()}>读取新进程样本</Button> : null}</div>}
  </section>;
}
