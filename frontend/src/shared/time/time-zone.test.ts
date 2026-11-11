import { describe, expect, it } from "vitest";
import { calendarDayRange, calendarDayStart, formatTimeParts, resolveTimeZone, shiftCalendarDate, timeZoneLabel } from "./time-zone";

const iso = (value: number) => new Date(value).toISOString();

describe("服务端时区", () => {
  it("显式时区格式化与浏览器本地时区无关，跨日不丢失日期", () => {
    const at = Date.parse("2026-11-10T16:30:00Z");
    expect(formatTimeParts(at, "Asia/Shanghai")).toEqual({ date: "2026-11-11", time: "00:30:00" });
    expect(formatTimeParts(at, "UTC")).toEqual({ date: "2026-11-10", time: "16:30:00" });
    expect(formatTimeParts(Number.NaN, "UTC")).toEqual({ date: "—", time: "—" });
    expect(timeZoneLabel("Asia/Shanghai", at)).toBe("Asia/Shanghai (UTC+08:00)");
  });

  it("缺失或无效时区明确回退 UTC，不读取浏览器默认时区", () => {
    expect(resolveTimeZone("Asia/Shanghai")).toEqual({ timeZone: "Asia/Shanghai", fallbackReason: null });
    expect(resolveTimeZone("UTC")).toEqual({ timeZone: "UTC", fallbackReason: null });
    expect(resolveTimeZone(undefined)).toEqual({ timeZone: "UTC", fallbackReason: "missing_timezone" });
    expect(resolveTimeZone("Not/AZone")).toEqual({ timeZone: "UTC", fallbackReason: "unsupported_timezone" });
  });

  it("上海自然日转换为正确的 UTC 半开区间", () => {
    expect(iso(calendarDayStart("2026-11-11", "Asia/Shanghai"))).toBe("2026-11-10T16:00:00.000Z");
    expect(iso(calendarDayStart("2026-11-12", "Asia/Shanghai"))).toBe("2026-11-11T16:00:00.000Z");
    const range = calendarDayRange(Date.parse("2026-11-10T16:30:00Z"), 7, "Asia/Shanghai");
    expect(iso(range.from_ms)).toBe("2026-11-04T16:00:00.000Z");
    expect(iso(range.to_ms)).toBe("2026-11-11T16:00:00.000Z");
  });

  it.each([
    ["2026-03-08", "2026-03-09", "2026-03-08T05:00:00.000Z", 23],
    ["2026-11-01", "2026-11-02", "2026-11-01T04:00:00.000Z", 25],
  ])("纽约 %s 按真实自然日计算而非固定 24 小时", (start, end, expected, hours) => {
    const from = calendarDayStart(start, "America/New_York");
    expect(iso(from)).toBe(expected);
    expect(calendarDayStart(end, "America/New_York") - from).toBe(hours * 3_600_000);
  });

  it("零点跳时的日期取当天首个有效时间点", () => {
    const start = calendarDayStart("2018-11-04", "America/Sao_Paulo");
    expect(iso(start)).toBe("2018-11-04T03:00:00.000Z");
    expect(formatTimeParts(start, "America/Sao_Paulo")).toEqual({ date: "2018-11-04", time: "01:00:00" });
  });

  it("偏移按历史时刻变化，不把当前偏移套用整个查询范围", () => {
    expect(timeZoneLabel("America/New_York", Date.parse("2026-01-01T12:00:00Z"))).toContain("UTC-05:00");
    expect(timeZoneLabel("America/New_York", Date.parse("2026-07-01T12:00:00Z"))).toContain("UTC-04:00");
  });

  it("无效日期不静默变成 epoch，公历移动正确跨月和闰年", () => {
    expect(calendarDayStart("", "UTC")).toBeNaN();
    expect(calendarDayStart("2026-02-30", "UTC")).toBeNaN();
    expect(shiftCalendarDate("2028-02-28", 1)).toBe("2028-02-29");
    expect(shiftCalendarDate("2026-12-31", 1)).toBe("2027-01-01");
  });
});
