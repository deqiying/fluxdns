const DAY_MS = 86_400_000;
const formatters = new Map<string, Intl.DateTimeFormat>();

function formatter(timeZone: string, kind: "date" | "clock" | "offset"): Intl.DateTimeFormat {
  const key = `${timeZone}:${kind}`;
  let value = formatters.get(key);
  if (!value) {
    const options: Intl.DateTimeFormatOptions = kind === "date"
      ? { year: "numeric", month: "2-digit", day: "2-digit" }
      : kind === "clock"
        ? { hour: "2-digit", minute: "2-digit", second: "2-digit", hourCycle: "h23" }
        : { timeZoneName: "longOffset" };
    value = new Intl.DateTimeFormat("en-CA", { ...options, timeZone });
    formatters.set(key, value);
  }
  return value;
}

/** 服务端只提供 IANA 时区；浏览器不支持或响应缺失时显式降级，绝不改用浏览器本地时区。 */
export function resolveTimeZone(value: unknown): { timeZone: string; fallbackReason: string | null } {
  if (typeof value !== "string" || !value) return { timeZone: "UTC", fallbackReason: "missing_timezone" };
  try {
    formatter(value, "date");
    return { timeZone: value, fallbackReason: null };
  } catch (error) {
    if (!(error instanceof RangeError)) throw error;
    return { timeZone: "UTC", fallbackReason: "unsupported_timezone" };
  }
}

/** 时间点按指定时区投影为稳定的公历日期与 24 小时时钟；不改动原始时间戳。 */
export function formatTimeParts(value: number, timeZone = "UTC"): { date: string; time: string } {
  if (!Number.isFinite(value) || Number.isNaN(new Date(value).getTime())) return { date: "—", time: "—" };
  const parts = formatter(timeZone, "date").formatToParts(value);
  const part = (type: Intl.DateTimeFormatPartTypes) => parts.find((item) => item.type === type)?.value;
  return { date: `${part("year")}-${part("month")}-${part("day")}`, time: formatter(timeZone, "clock").format(value) };
}

/** 偏移以对应时间点为准；简要模式只显示 UTC 偏移，完整模式保留 IANA 名称。 */
export function timeZoneLabel(timeZone: string, at: number, style: "full" | "offset" = "full"): string {
  if (timeZone === "UTC") return "UTC";
  const offset = formatter(timeZone, "offset").formatToParts(at).find((part) => part.type === "timeZoneName")?.value.replace("GMT", "UTC");
  if (style === "offset") return offset ?? timeZone;
  return `${timeZone}${offset ? ` (${offset})` : ""}`;
}

/** 在公历日期上移动，不把自然日当作固定 24 小时的时间段。 */
export function shiftCalendarDate(value: string, days: number): string {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return "";
  const parsed = Date.parse(`${value}T00:00:00Z`);
  if (!Number.isFinite(parsed) || new Date(parsed).toISOString().slice(0, 10) !== value) return "";
  return new Date(parsed + days * DAY_MS).toISOString().slice(0, 10);
}

/**
 * 求目标时区自然日的首个时间点。以公历日期二分查边界，兼容零点跳时和 23/25 小时日，
 * 不依赖浏览器本地时区，也不通过固定偏移反推；跳过的自然日与下一日使用同一边界。
 */
export function calendarDayStart(value: string, timeZone: string): number {
  if (shiftCalendarDate(value, 0) !== value || !value) return Number.NaN;
  const nominal = Date.parse(`${value}T00:00:00Z`);
  let low = nominal - 2 * DAY_MS;
  let high = nominal + 2 * DAY_MS;
  while (low < high) {
    const middle = Math.floor((low + high) / 2);
    if (formatTimeParts(middle, timeZone).date < value) low = middle + 1;
    else high = middle;
  }
  return low;
}

/** 包含今天在内的最近若干自然日，返回 API 所需 UTC 毫秒半开区间。 */
export function calendarDayRange(now: number, days: number, timeZone: string): { from_ms: number; to_ms: number } {
  const today = formatTimeParts(now, timeZone).date;
  return {
    from_ms: calendarDayStart(shiftCalendarDate(today, 1 - days), timeZone),
    to_ms: calendarDayStart(shiftCalendarDate(today, 1), timeZone),
  };
}
