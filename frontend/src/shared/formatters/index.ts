const integerFormatter = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 0 });
const decimalFormatter = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 2 });
const MEBIBYTE = 1_048_576n;
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

/** RSS 以 API 的十进制 u64 字符串接收，避免转换为 Number 后丢失精度。 */
export function formatBytesMiB(value: string | null | undefined): string {
  if (!value || !/^(0|[1-9][0-9]{0,19})$/.test(value)) return "—";
  const bytes = BigInt(value);
  if (bytes > U64_MAX) return "—";
  const tenths = (bytes * 10n + MEBIBYTE / 2n) / MEBIBYTE;
  const whole = tenths / 10n;
  const fraction = tenths % 10n;
  return `${integerFormatter.format(whole)}${fraction === 0n ? "" : `.${fraction}`} MiB`;
}
