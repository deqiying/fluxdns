const integerFormatter = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 0 });
const decimalFormatter = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 });
const U64_MAX = 18_446_744_073_709_551_615n;
const dateTimeFormatter = new Intl.DateTimeFormat("zh-CN", {
  timeZone: "UTC",
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hour12: false,
});

export function formatDateTime(value: string | null | undefined): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "—" : `${dateTimeFormatter.format(date)} UTC`;
}

export function formatEpochMillis(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isSafeInteger(value) || value < 0) return "—";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "—" : formatDateTime(date.toISOString());
}

export function formatCount(value: number | null | undefined): string {
  return value === null || value === undefined ? "—" : integerFormatter.format(value);
}

export function formatPercent(value: number | null | undefined): string {
  return value === null || value === undefined ? "—" : `${decimalFormatter.format(value)}%`;
}

export function formatDuration(value: number | null | undefined): string {
  return value === null || value === undefined ? "—" : `${decimalFormatter.format(value)} ms`;
}

export function formatUptime(seconds: number): string {
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600);
  const minutes = Math.floor((seconds % 3_600) / 60);
  return [days > 0 ? `${days} 天` : "", hours > 0 ? `${hours} 小时` : "", `${minutes} 分钟`]
    .filter(Boolean)
    .join(" ");
}

export function formatUptimeClock(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isSafeInteger(seconds) || seconds < 0) return "—";
  const days = Math.floor(seconds / 86_400);
  const hours = Math.floor((seconds % 86_400) / 3_600).toString().padStart(2, "0");
  const minutes = Math.floor((seconds % 3_600) / 60).toString().padStart(2, "0");
  const remainingSeconds = (seconds % 60).toString().padStart(2, "0");
  const clock = `${hours}:${minutes}:${remainingSeconds}`;
  return days > 0 ? `${days} 天 ${clock}` : clock;
}

/** 配置展示用的字节单位阶梯，按 1024 进制递进；标签与用户阅读习惯一致。 */
const BYTE_UNITS = [
  { label: "TB", factor: 1_099_511_627_776n },
  { label: "GB", factor: 1_073_741_824n },
  { label: "MB", factor: 1_048_576n },
  { label: "KB", factor: 1_024n },
  { label: "B", factor: 1n },
] as const;

/** 以 0.1 个单位为精度四舍五入，返回十倍值，避免浮点误差。 */
function roundTenths(bytes: bigint, factor: bigint): bigint {
  return (bytes * 10n + factor / 2n) / factor;
}

/** 十进制 u64 字符串或安全整数统一转 BigInt；超出 u64 或格式非法时返回 null。 */
function toByteCount(value: number | string | null | undefined): bigint | null {
  if (value === null || value === undefined) return null;
  if (typeof value === "number") {
    return Number.isSafeInteger(value) && value >= 0 ? BigInt(value) : null;
  }
  if (!/^(0|[1-9][0-9]{0,19})$/.test(value)) return null;
  const bytes = BigInt(value);
  return bytes > U64_MAX ? null : bytes;
}

/**
 * 字节数格式化：接受 API 的十进制 u64 字符串或安全整数，全程用 BigInt 避免精度丢失，
 * 按 1024 进制自适应到最大可读单位（B/KB/MB/GB/TB），整数不显示小数。
 * 进程 RSS 与配置字节字段统一使用这一套单位标签。
 */
export function formatBytes(value: number | string | null | undefined): string {
  const bytes = toByteCount(value);
  if (bytes === null) return "—";
  let index = BYTE_UNITS.findIndex((unit) => bytes >= unit.factor);
  if (index < 0) index = BYTE_UNITS.length - 1;
  let tenths = roundTenths(bytes, BYTE_UNITS[index].factor);
  // 四舍五入可能补足到 1024，此时进位到更大单位，避免出现「1,024 KB」这类边界写法。
  if (tenths >= 10_240n && index > 0) {
    index -= 1;
    tenths = roundTenths(bytes, BYTE_UNITS[index].factor);
  }
  const whole = tenths / 10n;
  const fraction = tenths % 10n;
  return `${integerFormatter.format(whole)}${fraction === 0n ? "" : `.${fraction}`} ${BYTE_UNITS[index].label}`;
}
