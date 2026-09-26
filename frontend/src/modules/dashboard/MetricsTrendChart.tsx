import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { Clock3 } from "lucide-react";
import type { ServiceMetrics } from "./api";
import { TREND_WINDOW_MS, type RateSample, type RateTrend } from "./rateTrend";

const HEIGHT = 252;
const LEFT = 42;
const RIGHT = 48;
const TOP = 18;
const BOTTOM = 34;
const PLOT_HEIGHT = HEIGHT - TOP - BOTTOM;
const timeFormatter = new Intl.DateTimeFormat("zh-CN", { timeZone: "UTC", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
/** 逐秒序列允许的最大相邻采样间隔，更宽的间隔按缺口处理。 */
const SAMPLE_GAP_MS = 1_500;

/** 双折线共享逐秒时间轴、各用线性刻度；RPM 由页面推导的过去 60 秒请求数给出，缺口不插值，键盘和指针共用选点状态。 */
export function MetricsTrendChart({ metrics, rateTrend }: { metrics: ServiceMetrics; rateTrend: RateTrend }) {
  const plotRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(1000);
  const plotWidth = width - LEFT - RIGHT;
  const startAt = metrics.sampled_at_ms - TREND_WINDOW_MS;
  const endAt = metrics.sampled_at_ms;
  const timeline = useMemo(
    () => [...new Set([...rateTrend.qps, ...rateTrend.rpm].map(({ at_ms }) => at_ms))]
      .filter((at) => at >= startAt && at <= endAt).sort((a, b) => a - b),
    [rateTrend.qps, rateTrend.rpm, startAt, endAt],
  );
  const [selectedAt, setSelectedAt] = useState<number | undefined>(() => timeline.at(-1));
  const [followLatest, setFollowLatest] = useState(true);
  const [showTooltip, setShowTooltip] = useState(false);
  const qpsMax = seriesMaximum(rateTrend.qps, startAt, endAt);
  const rpmMax = seriesMaximum(rateTrend.rpm, startAt, endAt);
  const selected = timeline.length === 0 ? undefined : followLatest || selectedAt === undefined ? timeline.at(-1) : nearestTime(timeline, selectedAt);
  const selectedX = selected === undefined ? 0 : xPosition(selected, startAt, endAt, width);
  const qpsSelected = nearestSample(rateTrend.qps, selected, startAt, endAt);
  const rpmSelected = nearestSample(rateTrend.rpm, selected, startAt, endAt);
  const hasValues = [...rateTrend.qps, ...rateTrend.rpm].some((sample) => sample.at_ms >= startAt && sample.at_ms <= endAt && sample.value.state === "available");
  const ticks = axisTicks(startAt, endAt, width);

  useEffect(() => {
    const plot = plotRef.current;
    if (!plot) return;
    // 以容器像素宽度作为 viewBox，窄屏文字和命中位置不随整幅 SVG 缩小。
    const observer = new ResizeObserver(([entry]) => {
      if (entry && entry.contentRect.width > 0) setWidth(Math.max(200, entry.contentRect.width));
    });
    observer.observe(plot);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (followLatest) setSelectedAt(timeline.at(-1));
  }, [followLatest, timeline]);

  const selectFromPointer = (event: PointerEvent<SVGSVGElement>) => {
    if (timeline.length === 0) return;
    const bounds = event.currentTarget.getBoundingClientRect();
    if (bounds.width === 0) return;
    const plotX = Math.max(0, Math.min(plotWidth, (event.clientX - bounds.left) / bounds.width * width - LEFT));
    const at = startAt + (plotX / plotWidth) * (endAt - startAt);
    setFollowLatest(false);
    setShowTooltip(true);
    setSelectedAt(nearestTime(timeline, at));
  };
  const selectFromKeyboard = (event: KeyboardEvent<HTMLDivElement>) => {
    if (timeline.length === 0 || !["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    setFollowLatest(false);
    setShowTooltip(true);
    const current = Math.max(0, timeline.indexOf(selected ?? timeline.at(-1)!));
    const index = event.key === "Home" ? 0
      : event.key === "End" ? timeline.length - 1
        : event.key === "ArrowLeft" ? Math.max(0, current - 1)
          : Math.min(timeline.length - 1, current + 1);
    setSelectedAt(timeline[index]);
  };

  return (
    <div
      className="metrics-chart"
      role="group"
      tabIndex={0}
      onKeyDown={selectFromKeyboard}
      onFocus={() => { setSelectedAt((value) => value ?? timeline.at(-1)); setShowTooltip(true); }}
      onBlur={() => { setFollowLatest(true); setShowTooltip(false); }}
      onPointerLeave={(event) => {
        if (document.activeElement !== event.currentTarget) { setFollowLatest(true); setShowTooltip(false); }
      }}
      aria-label={selected === undefined ? "最近十分钟没有趋势样本" : selectedLabel(selected, qpsSelected, rpmSelected)}
    >
      <div className="metrics-chart-heading">
        <div className="metrics-chart-title"><h3>请求趋势</h3><span>最近 10 分钟</span></div>
        <span className="metrics-chart-range"><Clock3 size={14} aria-hidden="true" />{formatTime(startAt)} – {formatTime(endAt)} UTC</span>
      </div>
      <div className="metrics-chart-legends">
        <div className="metrics-chart-legend"><i className="qps" /><strong>QPS</strong><span>请求/秒 · 左轴</span></div>
        <div className="metrics-chart-legend"><i className="rpm" /><strong>RPM</strong><span>请求/分钟 · 右轴</span></div>
      </div>
      <div className="metrics-chart-plot" ref={plotRef}>
        <svg viewBox={`0 0 ${width} ${HEIGHT}`} role="img" aria-labelledby="metrics-chart-title metrics-chart-description" onPointerMove={selectFromPointer} onPointerDown={selectFromPointer}>
          <title id="metrics-chart-title">QPS 与 RPM 请求趋势</title>
          <desc id="metrics-chart-description">最近十分钟 UTC 时间轴，逐秒一个采样点；左轴 QPS 为每秒请求量，右轴 RPM 为过去 60 秒的请求数，均为线性刻度；缺口处不连接，窗口最左 60 秒历史不足时不绘制 RPM。方向键按秒选择采样点，Home 和 End 跳转首尾。</desc>
          {[0, 1, 2, 3, 4].map((step) => {
            const y = TOP + PLOT_HEIGHT * step / 4;
            return <line key={step} className="metrics-chart-grid" x1={LEFT} x2={width - RIGHT} y1={y} y2={y} />;
          })}
          {[0, 1, 2, 3, 4].map((step) => {
            const y = TOP + PLOT_HEIGHT * step / 4 + 4;
            return (
              <g key={step}>
                <text className="metrics-chart-axis metrics-chart-axis-qps" x={LEFT - 10} y={y} textAnchor="end">{formatAxis(qpsMax * (4 - step) / 4)}</text>
                <text className="metrics-chart-axis metrics-chart-axis-rpm" x={width - RIGHT + 10} y={y}>{formatAxis(rpmMax * (4 - step) / 4)}</text>
              </g>
            );
          })}
          {seriesPaths(rateTrend.qps, startAt, endAt, qpsMax, width).map((path, index) => (
            <path key={`qps-${index}`} className="metrics-chart-qps-line" d={path} />
          ))}
          {seriesPaths(rateTrend.rpm, startAt, endAt, rpmMax, width).map((path, index) => (
            <path key={`rpm-${index}`} className="metrics-chart-rpm-line" d={path} />
          ))}
          {selected !== undefined ? <line className="metrics-chart-cursor" x1={selectedX} x2={selectedX} y1={TOP} y2={TOP + PLOT_HEIGHT} /> : null}
          {[{ sample: qpsSelected, maximum: qpsMax, name: "qps" }, { sample: rpmSelected, maximum: rpmMax, name: "rpm" }].map(({ sample, maximum, name }) => sample?.value.state === "available" ? (
            <circle key={name} className={`metrics-chart-point metrics-chart-point-${name}`} cx={xPosition(sample.at_ms, startAt, endAt, width)} cy={TOP + PLOT_HEIGHT - sample.value.value / maximum * PLOT_HEIGHT} r={4} />
          ) : null)}
          {ticks.map((at) => (
            <text key={at} className="metrics-chart-axis" x={xPosition(at, startAt, endAt, width)} y={HEIGHT - 10} textAnchor={at === startAt ? "start" : at === endAt ? "end" : "middle"}>{formatTime(at).slice(0, 5)}</text>
          ))}
        </svg>
        {showTooltip && selected !== undefined ? (
          <div className="metrics-chart-tooltip" style={{ left: Math.max(100, Math.min(width - 100, selectedX)) }}>
            <strong>{formatTime(selected)} UTC</strong>
            <span><i className="qps" /><span>QPS</span><b>{formatSample(qpsSelected)}</b></span>
            <span><i className="rpm" /><span>RPM</span><b>{formatSample(rpmSelected)}</b></span>
          </div>
        ) : null}
        {!hasValues ? <div className="metrics-chart-empty">{timeline.length === 0 ? "最近十分钟暂无趋势样本" : "趋势样本暂不可用，等待有效采样"}</div> : null}
      </div>
      <div className="metrics-chart-footer">逐秒采样 · RPM 为过去 60 秒请求数 · 悬停或点按查看</div>
    </div>
  );
}

/** 为当前窗口预留顶部空间，并使四段线性刻度落在易读的数值上。 */
function seriesMaximum(samples: RateSample[], startAt: number, endAt: number): number {
  const maximum = Math.max(0, ...samples.flatMap(({ value, at_ms }) => value.state === "available" && at_ms >= startAt && at_ms <= endAt ? [value.value] : []));
  if (maximum === 0) return 1;
  const step = maximum * 1.1 / 4;
  const magnitude = 10 ** Math.floor(Math.log10(step));
  const rounded = [1, 2, 4, 5, 10].find((value) => value * magnitude >= step) ?? 10;
  return rounded * magnitude * 4;
}

/** 刻度标签按分钟给出，窄屏逐级放宽到 2/5 分钟；始终保留窗口首尾两个边界标签。 */
function axisTicks(startAt: number, endAt: number, width: number): number[] {
  const step = width < 480 ? 300_000 : width < 720 ? 120_000 : 60_000;
  const ticks: number[] = [];
  for (let at = startAt; at < endAt; at += step) ticks.push(at);
  ticks.push(endAt);
  return ticks;
}

/** 不跨不可用区间或缺失秒连接；孤立样本用零长度线段配合圆端帽保留可见点。 */
function seriesPaths(samples: RateSample[], startAt: number, endAt: number, maximum: number, width: number): string[] {
  const paths: string[] = [];
  let points: string[] = [];
  let previousAt: number | undefined;
  for (const sample of samples) {
    if (sample.value.state !== "available" || sample.at_ms < startAt || sample.at_ms > endAt) {
      if (points.length > 0) paths.push(samplePath(points));
      points = [];
      previousAt = undefined;
      continue;
    }
    // 相邻可用样本间距超过 1.5 秒说明中间缺秒（例如跨后端实例的秒点网格），按缺口断开而不插值。
    if (points.length > 0 && previousAt !== undefined && sample.at_ms - previousAt > SAMPLE_GAP_MS) {
      paths.push(samplePath(points));
      points = [];
    }
    const x = xPosition(sample.at_ms, startAt, endAt, width);
    const y = TOP + PLOT_HEIGHT - sample.value.value / maximum * PLOT_HEIGHT;
    points.push(`${x.toFixed(2)} ${y.toFixed(2)}`);
    previousAt = sample.at_ms;
  }
  if (points.length > 0) paths.push(samplePath(points));
  return paths;
}

function samplePath(points: string[]): string {
  return `M ${points.join(" L ")}${points.length === 1 ? ` L ${points[0]}` : ""}`;
}

function xPosition(at: number, startAt: number, endAt: number, width: number): number {
  return LEFT + Math.max(0, Math.min(1, (at - startAt) / Math.max(1, endAt - startAt))) * (width - LEFT - RIGHT);
}

function nearestTime(values: number[], target: number): number {
  return values.reduce((best, value) => Math.abs(value - target) < Math.abs(best - target) ? value : best);
}

function nearestSample(samples: RateSample[], at: number | undefined, startAt: number, endAt: number): RateSample | undefined {
  const visible = samples.filter((sample) => sample.at_ms >= startAt && sample.at_ms <= endAt);
  if (at === undefined || visible.length === 0) return undefined;
  return visible.reduce((best, value) => Math.abs(value.at_ms - at) < Math.abs(best.at_ms - at) ? value : best);
}

function formatSample(sample: RateSample | undefined): string {
  if (!sample || sample.value.state !== "available") return "暂不可用";
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(sample.value.value);
}

function formatAxis(value: number): string {
  return new Intl.NumberFormat("zh-CN", { maximumSignificantDigits: 3, notation: value >= 10_000 ? "compact" : "standard" }).format(value);
}

function formatTime(at: number): string {
  return timeFormatter.format(at);
}

function selectedLabel(at: number, qps: RateSample | undefined, rpm: RateSample | undefined): string {
  return `${formatTime(at)} UTC，QPS ${formatSample(qps)}，RPM ${formatSample(rpm)}`;
}
