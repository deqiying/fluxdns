import { describe, expect, it } from "vitest";
import {
  BYTE_DISPLAY_MAX_FRACTION_DIGITS,
  BYTE_UNIT_OPTIONS,
  bytesFromForm,
  bytesToDisplay,
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
  it("字节按 1024 进制精确往返并执行 schema 上限", () => {
    expect(bytesFromForm("1.5", "MB")).toBe(1_572_864);
    expect(bytesToForm(1_572_864, "MB")).toBe("1.5");
    expect(bytesFromForm("1024", "GB")).toBe(1_099_511_627_776);
    expect(() => bytesFromForm("1024.000000001", "GB")).toThrow();
    expect(() => bytesFromForm("0", "B")).toThrow();
    // 单位一律 1024 进制：KB 是 1024 B，而不是十进制 kB。
    expect(bytesFromForm("1", "KB")).toBe(1_024);
    expect(bytesToForm(1_024, "KB")).toBe("1");
    // TB 与 schema 上限同量级：1 TB 合法，1.5 TB 越界。
    expect(bytesFromForm("1", "TB")).toBe(1_099_511_627_776);
    expect(bytesToForm(1_099_511_627_776, "TB")).toBe("1");
    expect(() => bytesFromForm("1.5", "TB")).toThrow();
    // 不足一个字节的字节数无法精确表示，必须在换算阶段拒绝。
    expect(() => bytesFromForm("1.5", "B")).toThrow();
  });

  it("字节回显挑选能精确表示的最大单位，保证无损且小数不超过上限", () => {
    expect(BYTE_UNIT_OPTIONS).toEqual([
      { value: "B", label: "B" },
      { value: "KB", label: "KB" },
      { value: "MB", label: "MB" },
      { value: "GB", label: "GB" },
      { value: "TB", label: "TB" },
    ]);
    // 回显小数位上限是显式常量：从 2 位放宽到 6 位，避免 807306368 这类值只能回落成整数字节。
    expect(BYTE_DISPLAY_MAX_FRACTION_DIGITS).toBe(6);
    // 单位阶梯从大到小，但只考虑数值 ≥ 1 的单位：64 MiB 优先 64 MB，而不是更难受的 0.0625 GB。
    expect(bytesToDisplay(67_108_864)).toEqual({ value: "64", unit: "MB" });
    expect(bytesToDisplay(1_073_741_824)).toEqual({ value: "1", unit: "GB" });
    // 1 TiB 回显为 1 TB，不再退化成 1024 GB。
    expect(bytesToDisplay(1_099_511_627_776)).toEqual({ value: "1", unit: "TB" });
    // 小数回显与摘要 formatBytes 口径一致：1.5 MB 而不是 1536 KB，也不是 0.0014 GB。
    expect(bytesToDisplay(1_572_864)).toEqual({ value: "1.5", unit: "MB" });
    expect(bytesToDisplay(1_536)).toEqual({ value: "1.5", unit: "KB" });
    // 数值 ≥ 1 且能精确表示时优先选更大单位：768 MiB 回显 768 MB（0.75 GB 数值不足 1，不采用）。
    expect(bytesToDisplay(805_306_368)).toEqual({ value: "768", unit: "MB" });
    expect(bytesToDisplay(1)).toEqual({ value: "1", unit: "B" });
    // 807306368 B 在 MB 下需要 10 位小数（769.9072265625），超过 6 位上限；同为大单位的 GB 数值不足 1，
    // 因此退到 KB（3 位小数），仍然无损且可读，而不是回落成 `807306368 B`。
    expect(bytesToDisplay(807_306_368)).toEqual({ value: "788385.125", unit: "KB" });
    // 4 位小数够用时仍选更大单位：807337984 B = 769.9375 MB。
    expect(bytesToDisplay(807_337_984)).toEqual({ value: "769.9375", unit: "MB" });
    // 极端值需要 10 位小数（1048577 B = 1024.0009765625 KB），超过上限只能回落成整数 B。
    expect(bytesToDisplay(1_048_577)).toEqual({ value: "1048577", unit: "B" });
    expect(() => bytesToDisplay(0)).toThrow();
    expect(() => bytesToDisplay(1_099_511_627_777)).toThrow();
    expect(() => bytesToDisplay(1.5)).toThrow();
  });

  it("字节回显可无损换算回原始字节数，且小数位不超过上限", () => {
    const samples = [
      1, 1_024, 1_536, 1_048_576, 1_048_577, 1_572_864, 67_108_864, 805_306_368, 807_306_368, 807_337_984,
      1_073_741_824, 1_099_511_627_776,
    ];
    // 稀疏扫描覆盖到 schema 上限：无损往返只依赖单位阶梯，不应挑值成立。
    const scanned: number[] = [...samples];
    for (let bytes = 1_048_577; bytes <= 1_099_511_627_776; bytes += 33_554_432) scanned.push(bytes);
    for (const bytes of scanned) {
      const { value, unit } = bytesToDisplay(bytes);
      const label = `${bytes} → ${value} ${unit}`;
      expect(bytesFromForm(value, unit), label).toBe(bytes);
      // 逆运算同样精确：回显文本就是该单位下的精确表示，不含四舍五入。
      expect(bytesToForm(bytes, unit), label).toBe(value);
      expect(value.split(".")[1]?.length ?? 0, label).toBeLessThanOrEqual(BYTE_DISPLAY_MAX_FRACTION_DIGITS);
      // 数值 ≥ 1：可读性规则不允许回显成 0.0625 GB 这类写法。
      expect(Number(value), label).toBeGreaterThanOrEqual(1);
    }
    // 1..4096 逐字节穷举，覆盖 B/KB 进位边界。
    for (let bytes = 1; bytes <= 4_096; bytes += 1) {
      const { value, unit } = bytesToDisplay(bytes);
      expect(bytesFromForm(value, unit), `${bytes} → ${value} ${unit}`).toBe(bytes);
    }
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
