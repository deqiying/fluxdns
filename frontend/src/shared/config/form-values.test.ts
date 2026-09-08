import { describe, expect, it } from "vitest";
import {
  bytesFromForm,
  bytesToForm,
  deserializeInheritance,
  durationFromNanoseconds,
  durationToNanoseconds,
  isIpOrCidr,
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
