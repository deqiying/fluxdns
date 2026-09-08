import { useEffect, useMemo, useState, type KeyboardEvent, type PointerEvent } from "react";
import type { ServiceMetrics } from "./api";

type RateSample = ServiceMetrics["qps_trend"][number];

const WIDTH = 1000;
const HEIGHT = 320;
const LEFT = 62;
const RIGHT = 74;
const TOP = 28;
const BOTTOM = 42;
const PLOT_WIDTH = WIDTH - LEFT - RIGHT;
const PLOT_HEIGHT = HEIGHT - TOP - BOTTOM;

export function MetricsTrendChart({ metrics }: { metrics: ServiceMetrics }) {
  const timeline = useMemo(
    () => [...new Set([...metrics.qps_trend, ...metrics.rpm_trend].map(({ at_ms }) => at_ms))].sort((a, b) => a - b),
    [metrics.qps_trend, metrics.rpm_trend],
  );
  const [selectedAt, setSelectedAt] = useState<number | undefined>(() => timeline.at(-1));
  const [followLatest, setFollowLatest] = useState(true);
  const startAt = metrics.sampled_at_ms - 10 * 60_000;
  const endAt = metrics.sampled_at_ms;
  const qpsMax = seriesMaximum(metrics.qps_trend);
  const rpmMax = seriesMaximum(metrics.rpm_trend);
  const selected = selectedAt ?? timeline.at(-1);
  const selectedX = selected === undefined ? 0 : xPosition(selected, startAt, endAt);
  const qpsSelected = nearestSample(metrics.qps_trend, selected);
  const rpmSelected = nearestSample(metrics.rpm_trend, selected);

  useEffect(() => {
    if (followLatest) setSelectedAt(timeline.at(-1));
  }, [followLatest, timeline]);

  const selectFromPointer = (event: PointerEvent<SVGSVGElement>) => {
    if (timeline.length === 0) return;
    const bounds = event.currentTarget.getBoundingClientRect();
    const plotX = Math.max(0, Math.min(PLOT_WIDTH, (event.clientX - bounds.left) / bounds.width * WIDTH - LEFT));
    const at = startAt + (plotX / PLOT_WIDTH) * (endAt - startAt);
    setFollowLatest(false);
    setSelectedAt(nearestTime(timeline, at));
  };
  const selectFromKeyboard = (event: KeyboardEvent<HTMLDivElement>) => {
    if (timeline.length === 0 || !["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    setFollowLatest(false);
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
      onFocus={() => setSelectedAt((value) => value ?? timeline.at(-1))}
      onBlur={() => setFollowLatest(true)}
      onPointerLeave={() => setFollowLatest(true)}
      aria-label={selected === undefined ? "最近十分钟没有趋势样本" : selectedLabel(selected, qpsSelected, rpmSelected)}
    >
      <div className="metrics-chart-heading">
        <div className="metrics-chart-legend"><i className="qps" />QPS · 请求/秒</div>
        <div className="metrics-chart-legend"><i className="rpm" />RPM · 请求/分钟</div>
      </div>
      <div className="metrics-chart-plot">
        <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="img" aria-labelledby="metrics-chart-title metrics-chart-description" onPointerMove={selectFromPointer}>
          <title id="metrics-chart-title">QPS 与 RPM 请求趋势</title>
          <desc id="metrics-chart-description">最近十分钟共同时间轴，左轴为每秒请求量，右轴为每分钟请求量；缺口处不连接。</desc>
          {[0, 1, 2, 3, 4].map((step) => {
            const y = TOP + PLOT_HEIGHT * step / 4;
            return <line key={step} className="metrics-chart-grid" x1={LEFT} x2={WIDTH - RIGHT} y1={y} y2={y} />;
          })}
          {[0, 1, 2, 3, 4].map((step) => {
            const y = TOP + PLOT_HEIGHT * step / 4 + 4;
            return (
              <g key={step}>
                <text className="metrics-chart-axis" x={LEFT - 10} y={y} textAnchor="end">{formatAxis(qpsMax * (4 - step) / 4)}</text>
                <text className="metrics-chart-axis" x={WIDTH - RIGHT + 10} y={y}>{formatAxis(rpmMax * (4 - step) / 4)}</text>
              </g>
            );
          })}
          {seriesPaths(metrics.qps_trend, startAt, endAt, qpsMax).map((path, index) => (
            <g key={`qps-${index}`}>
              <path className="metrics-chart-qps-area" d={`${path} L ${pathEndX(path)} ${TOP + PLOT_HEIGHT} L ${pathStartX(path)} ${TOP + PLOT_HEIGHT} Z`} />
              <path className="metrics-chart-qps-line" d={path} />
            </g>
          ))}
          {seriesPaths(metrics.rpm_trend, startAt, endAt, rpmMax).map((path, index) => (
            <path key={`rpm-${index}`} className="metrics-chart-rpm-line" d={path} />
          ))}
          {selected !== undefined ? <line className="metrics-chart-cursor" x1={selectedX} x2={selectedX} y1={TOP} y2={TOP + PLOT_HEIGHT} /> : null}
          {[startAt, startAt + 5 * 60_000, endAt].map((at) => (
            <text key={at} className="metrics-chart-axis" x={xPosition(at, startAt, endAt)} y={HEIGHT - 12} textAnchor={at === startAt ? "start" : at === endAt ? "end" : "middle"}>{formatTime(at)}</text>
          ))}
        </svg>
        {selected !== undefined ? (
          <div className="metrics-chart-tooltip" style={{ left: `${Math.max(8, Math.min(78, selectedX / WIDTH * 100))}%` }}>
            <strong>{formatTime(selected)}</strong>
            <span><i className="qps" />QPS {formatSample(qpsSelected)}</span>
            <span><i className="rpm" />RPM {formatSample(rpmSelected)}</span>
          </div>
        ) : null}
      </div>
    </div>
  );
}

function seriesMaximum(samples: RateSample[]): number {
  const maximum = Math.max(0, ...samples.flatMap(({ value }) => value.state === "available" ? [value.value] : []));
  return maximum > 0 ? maximum * 1.1 : 1;
}

function seriesPaths(samples: RateSample[], startAt: number, endAt: number, maximum: number): string[] {
  const paths: string[] = [];
  let points: string[] = [];
  for (const sample of samples) {
    if (sample.value.state !== "available" || sample.at_ms < startAt || sample.at_ms > endAt) {
      if (points.length > 0) paths.push(`M ${points.join(" L ")}`);
      points = [];
      continue;
    }
    const x = xPosition(sample.at_ms, startAt, endAt);
    const y = TOP + PLOT_HEIGHT - sample.value.value / maximum * PLOT_HEIGHT;
    points.push(`${x.toFixed(2)} ${y.toFixed(2)}`);
  }
  if (points.length > 0) paths.push(`M ${points.join(" L ")}`);
  return paths;
}

function pathStartX(path: string): string {
  return path.split(" ")[1] ?? String(LEFT);
}

function pathEndX(path: string): string {
  const values = path.split(" L ").at(-1)?.trim().split(" ");
  return values?.[0] ?? String(LEFT);
}

function xPosition(at: number, startAt: number, endAt: number): number {
  return LEFT + Math.max(0, Math.min(1, (at - startAt) / Math.max(1, endAt - startAt))) * PLOT_WIDTH;
}

function nearestTime(values: number[], target: number): number {
  return values.reduce((best, value) => Math.abs(value - target) < Math.abs(best - target) ? value : best);
}

function nearestSample(samples: RateSample[], at: number | undefined): RateSample | undefined {
  if (at === undefined || samples.length === 0) return undefined;
  return samples.reduce((best, value) => Math.abs(value.at_ms - at) < Math.abs(best.at_ms - at) ? value : best);
}

function formatSample(sample: RateSample | undefined): string {
  if (!sample || sample.value.state !== "available") return "暂不可用";
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 }).format(sample.value.value);
}

function formatAxis(value: number): string {
  return new Intl.NumberFormat("zh-CN", { maximumFractionDigits: value < 10 ? 1 : 0, notation: value >= 10_000 ? "compact" : "standard" }).format(value);
}

function formatTime(at: number): string {
  return new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function selectedLabel(at: number, qps: RateSample | undefined, rpm: RateSample | undefined): string {
  return `${formatTime(at)}，QPS ${formatSample(qps)}，RPM ${formatSample(rpm)}`;
}
