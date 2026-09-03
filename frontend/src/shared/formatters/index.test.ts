import { describe, expect, it } from "vitest";
import { formatCount, formatDateTime, formatDuration, formatPercent, formatUptime } from ".";

describe("formatters", () => {
  it("按 UTC 确定性格式化时间", () => {
    expect(formatDateTime("2026-09-03T08:09:10Z")).toContain("2026");
    expect(formatDateTime("invalid")).toBe("—");
  });

  it("格式化数值与运行时长", () => {
    expect(formatCount(1234)).toMatch(/1[,，]?234/);
    expect(formatPercent(86.47)).toBe("86.47%");
    expect(formatDuration(2.5)).toBe("2.5 ms");
    expect(formatUptime(93_780)).toBe("1 天 2 小时 3 分钟");
  });
});
