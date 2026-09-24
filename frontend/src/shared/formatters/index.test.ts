import { describe, expect, it } from "vitest";
import {
  formatBytes,
  formatBytesMiB,
  formatCount,
  formatDateTime,
  formatDuration,
  formatEpochMillis,
  formatPercent,
  formatUptime,
  formatUptimeClock,
} from ".";

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
    expect(formatUptimeClock(282_258)).toBe("3 天 06:24:18");
    expect(formatUptimeClock(-1)).toBe("—");
  });

  it("从安全时间戳和十进制 u64 格式化采样时间与 MiB", () => {
    expect(formatEpochMillis(Date.parse("2026-09-03T08:09:10Z"))).toContain("2026");
    expect(formatEpochMillis(Number.MAX_SAFE_INTEGER + 1)).toBe("—");
    expect(formatBytesMiB("195454566")).toBe("186.4 MiB");
    expect(formatBytesMiB("18446744073709551615")).toMatch(/MiB$/);
    expect(formatBytesMiB("18446744073709551616")).toBe("—");
    expect(formatBytesMiB("01")).toBe("—");
  });
  it("按人类可读单位格式化字节数", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1024)).toBe("1 KB");
    expect(formatBytes(32_768)).toBe("32 KB");
    expect(formatBytes(67_108_864)).toBe("64 MB");
    expect(formatBytes("805306368")).toBe("768 MB");
    expect(formatBytes("195454566")).toBe("186.4 MB");
    expect(formatBytes(1_073_741_824)).toBe("1 GB");
    // 四舍五入到 1024 个单位时进位到更大单位，避免出现「1,024 MB」。
    expect(formatBytes(1_073_689_396)).toBe("1 GB");
    expect(formatBytes(null)).toBe("—");
    expect(formatBytes(-1)).toBe("—");
    expect(formatBytes(Number.MAX_SAFE_INTEGER + 1)).toBe("—");
    expect(formatBytes("01")).toBe("—");
    expect(formatBytes("18446744073709551616")).toBe("—");
  });
});
