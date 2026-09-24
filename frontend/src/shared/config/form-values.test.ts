import { describe, expect, it } from "vitest";
import {
  bytesFromForm,
  bytesToForm,
  deserializeInheritance,
  durationFromForm,
  durationFromNanoseconds,
  durationToForm,
  durationToNanoseconds,
  formatDurationText,
  isIpOrCidr,
  isNonNegativeDuration,
  isPositiveDuration,
  normalizeDuration,
  referenceOptions,
  selectVariantPayload,
  serializeInheritance,
} from "./form-values";

describe("配置表单共享值转换", () => {
  it("字节与二进制单位精确往返并执行 schema 上限", () => {
    expect(bytesFromForm("1.5", "MiB")).toBe(1_572_864);
    expect(bytesToForm(1_572_864, "MiB")).toBe("1.5");
    expect(bytesFromForm("1024", "GiB")).toBe(1_099_511_627_776);
    expect(() => bytesFromForm("1024.000000001", "GiB")).toThrow();
    expect(() => bytesFromForm("0", "B")).toThrow();
  });

  it("复合 duration 不经 Number 丢失精度", () => {
    const value = durationToNanoseconds("1h30m250.5ms");
    expect(value).toBe(5_400_250_500_000n);
    expect(durationFromNanoseconds(value)).toBe("1h30m250ms500us");
    for (const invalid of ["", " 1s", "1", "1xs", "0.1ns"]) {
      expect(() => durationToNanoseconds(invalid)).toThrow();
    }
  });

  it("时长表单按秒回显且只提供可感知单位", () => {
    expect(durationToForm("5000000000ns")).toEqual({ amount: "5", unit: "s" });
    expect(durationToForm("86400000000000ns")).toEqual({ amount: "1", unit: "d" });
    expect(durationToForm("3000000000ns")).toEqual({ amount: "3", unit: "s" });
    // 非整秒回退到毫秒（仍是可感知量级），绝不下沉到 us/ns。
    expect(durationToForm("1500000000ns")).toEqual({ amount: "1500", unit: "ms" });
    expect(durationToForm("500000ns")).toEqual({ amount: "0.5", unit: "ms" });
    expect(durationToForm("300000000000ns")).toEqual({ amount: "5", unit: "m" });
    expect(durationToForm("90000000000000ns")).toEqual({ amount: "25", unit: "h" });
    expect(durationToForm("3600000000000ns")).toEqual({ amount: "1", unit: "h" });
    expect(durationToForm(undefined)).toEqual({ amount: "", unit: "s" });

    expect(durationFromForm("5", "s")).toBe("5s");
    expect(durationFromForm("1.50", "s")).toBe("1.5s");
    expect(durationFromForm("1500", "ms")).toBe("1500ms");
    expect(durationFromForm("2", "m")).toBe("2m");
    expect(durationFromForm("1.5", "h")).toBe("1.5h");
    expect(() => durationFromForm("0.0000001", "ms")).toThrow();
    // 分钟/小时的最小可表示量级可能超过后端 9 位小数上限，必须在组合阶段就拒绝。
    expect(durationFromForm("0.000000001", "m")).toBe("0.000000001m");
    expect(() => durationFromForm("0.0000000001", "m")).toThrow();
    expect(() => durationFromForm("", "s")).toThrow();

    // 回填归一化：未编辑字段也不会把纳秒串写回变更报文。
    expect(normalizeDuration("5000000000ns")).toBe("5s");
    expect(normalizeDuration("1500000000ns")).toBe("1500ms");
    expect(normalizeDuration(undefined)).toBeUndefined();
    expect(formatDurationText("300000000000ns")).toBe("5 分钟");
    expect(formatDurationText("86400000000000ns")).toBe("1 天");
    expect(formatDurationText("5000000000ns")).toBe("5 秒");
    expect(formatDurationText("1500000000ns")).toBe("1500 毫秒");
    expect(formatDurationText("bogus")).toBe("bogus");
    expect(formatDurationText(undefined)).toBe("");
    expect(normalizeDuration("bogus")).toBe("bogus");
  });

  it("时长合法性判定区分必填正数与可为零的边界", () => {
    for (const valid of ["5s", "1500ms", "1h30m", "0.5s"]) {
      expect(isPositiveDuration(valid), valid).toBe(true);
    }
    for (const invalid of [undefined, "", "0s", "0ms", "abc", "5"]) {
      expect(isPositiveDuration(invalid), String(invalid)).toBe(false);
    }
    // TTL 上下限的 0s 表示该边界不设限，必须与必填字段的正数要求区分开。
    expect(durationFromForm("0", "s")).toBe("0s");
    expect(isPositiveDuration("0s")).toBe(false);
    for (const boundary of ["0s", "0ms", "5s", "1d"]) {
      expect(isNonNegativeDuration(boundary), boundary).toBe(true);
    }
    for (const invalid of [undefined, "", "abc", "5"]) {
      expect(isNonNegativeDuration(invalid), String(invalid)).toBe(false);
    }
  });

  it("IP/CIDR 词法检查覆盖双栈和前缀边界", () => {
    for (const valid of ["192.0.2.1", "192.0.2.0/24", "2001:db8::1", "2001:db8::/64"]) {
      expect(isIpOrCidr(valid), valid).toBe(true);
    }
    for (const invalid of ["192.0.2.01", "256.0.0.1", "192.0.2.0/33", "2001:db8::/129", "host.test"]) {
      expect(isIpOrCidr(invalid), invalid).toBe(false);
    }
  });

  it("继承、缺失引用和 variant 隐藏字段保持不同语义", () => {
    expect(deserializeInheritance(undefined)).toEqual({ kind: "inherit" });
    expect(serializeInheritance({ kind: "value", value: { enabled: false } })).toEqual({ enabled: false });
    expect(referenceOptions(["primary", "backup"], "removed")[0]).toEqual({
      value: "removed",
      label: "removed（已不存在）",
      disabled: true,
      missing: true,
    });
    const source = { type: "remote" as const, name: "rules", url: "https://example.test", path: "stale" };
    expect(selectVariantPayload(source, "remote", ["type", "name", "url"])).toEqual({
      type: "remote",
      name: "rules",
      url: "https://example.test",
    });
  });
});
