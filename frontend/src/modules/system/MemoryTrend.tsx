import { useState } from "react";
import { formatEpochMillis } from "@/shared/formatters";
import { useServerTimeZone } from "@/shared/time/TimeZoneContext";
import type { MemoryAnalysis } from "./api";
import { formatMemoryBytes, memoryTrendSegments, type TrendKey } from "./memory-model";

const series: { key: TrendKey; label: string; color: string }[] = [
  { key: "rss_bytes", label: "RSS", color: "#007aff" },
  { key: "anonymous_bytes", label: "匿名内存", color: "#729cdd" },
  { key: "file_bytes", label: "文件映射", color: "#19a6a0" },
];

/** 仅绘制服务端真实样本，所有曲线共享同一 MiB 纵轴。 */
export function MemoryTrend({ data }: { data: MemoryAnalysis }) {
  const { timeZone } = useServerTimeZone();
  const [minutes, setMinutes] = useState(10);
  const [hidden, setHidden] = useState<TrendKey[]>([]);
  const end = data.os.sampled_at_ms;
  const start = Math.max(0, end - minutes * 60_000);
  const samples = data.trend.filter((sample) => sample.sampled_at_ms >= start && sample.sampled_at_ms <= end);
  const curves = series.filter(({ key }) => !hidden.includes(key)).map((item) => ({
    ...item, segments: memoryTrendSegments(samples, item.key, data.gap_after_seconds),
  }));
  const points = curves.flatMap((curve) => curve.segments.flat());
  const maxBytes = points.reduce((max, point) => point.bytes > max ? point.bytes : max, 1n);
  const x = (at: number) => 80 + ((at - start) / Math.max(1, end - start)) * 890;
  // 先用 BigInt 求有界比例，再转 SVG 坐标，绝不将原始 u64 转 Number。
  const y = (bytes: bigint) => 190 - Number(bytes * 160_000n / maxBytes) / 1_000;
  const toggle = (key: TrendKey) => setHidden((keys) => keys.includes(key) ? keys.filter((item) => item !== key) : [...keys, key]);

  return (
    <div className="memory-trend-panel">
      <div className="memory-trend-toolbar">
        <div className="memory-series" aria-label="选择趋势曲线">
          {series.map(({ key, label, color }) => <button key={key} type="button" aria-pressed={!hidden.includes(key)} onClick={() => toggle(key)}><i style={{ background: color }} />{label}</button>)}
        </div>
        <div className="memory-range" aria-label="趋势范围">
          {[10, 60].map((range) => <button key={range} type="button" aria-pressed={minutes === range} onClick={() => setMinutes(range)}>{range === 10 ? "10 分钟" : "1 小时"}</button>)}
        </div>
      </div>
      {points.length ? (
        <svg className="memory-trend-svg" viewBox="0 0 1000 230" role="img" aria-label="进程内存变化趋势，RSS、匿名内存与文件映射共享 MiB 纵轴">
          <title>进程内存变化趋势</title>
          <desc>仅显示真实样本。缺测、时间倒序或间隔超过 {data.gap_after_seconds} 秒时断线；不补数据。</desc>
          {[0, 1, 2, 3, 4].map((tick) => {
            const value = maxBytes * BigInt(4 - tick) / 4n;
            const at = 30 + tick * 40;
            return <g key={tick}><line x1="80" x2="970" y1={at} y2={at} stroke="#edf0f4" /><text x="72" y={at + 4} textAnchor="end">{formatMemoryBytes(value)}</text></g>;
          })}
          {curves.map(({ key, color, segments }) => <g key={key} data-series={key}>
            {segments.map((segment, index) => segment.length === 1
              ? <circle key={index} cx={x(segment[0].at)} cy={y(segment[0].bytes)} r="2.5" fill={color} />
              : <path key={index} d={segment.map((point, i) => `${i === 0 ? "M" : "L"}${x(point.at).toFixed(2)},${y(point.bytes).toFixed(2)}`).join(" ")} fill="none" stroke={color} strokeWidth="2" />)}
          </g>)}
          <text x="80" y="218">{minutes === 10 ? "−10 分钟" : "−1 小时"}</text><text x="970" y="218" textAnchor="end">最新 OS 样本</text>
        </svg>
      ) : <p className="memory-empty">当前范围没有可用样本或未选择曲线。</p>}
      <div className="memory-trend-caption"><span>{formatEpochMillis(start, timeZone)} — {formatEpochMillis(end, timeZone)}</span><span>{samples.length} 个真实样本 · 数据不足不补齐</span></div>
    </div>
  );
}
