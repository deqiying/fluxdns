export type ByteUnit = "B" | "KB" | "MB" | "GB" | "TB";

/** 字节单位一律按 1024 进制递进，标签与展示层 formatBytes 保持同一套口径。 */
const BYTE_FACTORS: Record<ByteUnit, bigint> = {
  B: 1n,
  KB: 1_024n,
  MB: 1_048_576n,
  GB: 1_073_741_824n,
  TB: 1_099_511_627_776n,
};
const MAX_BYTES = 1_099_511_627_776n;

const DURATION_FACTORS = {
  w: 604_800_000_000_000n,
  d: 86_400_000_000_000n,
  h: 3_600_000_000_000n,
  m: 60_000_000_000n,
  s: 1_000_000_000n,
  ms: 1_000_000n,
  us: 1_000n,
  ns: 1n,
} as const;

export type Inheritable<T> = { kind: "inherit" } | { kind: "value"; value: T };

export interface ReferenceOption {
  value: string;
  label: string;
  disabled?: boolean;
  missing?: boolean;
}

/** 字节换算全程使用 BigInt，只有确认满足 OpenAPI Bytes 上限后才返回 number。 */
export function bytesFromForm(value: string, unit: ByteUnit): number {
  if (value.length > 64) throw new Error("invalid decimal");
  const bytes = parseExactDecimal(value, BYTE_FACTORS[unit]);
  if (bytes < 1n || bytes > MAX_BYTES) throw new Error("bytes out of range");
  return Number(bytes);
}

export function bytesToForm(bytes: number, unit: ByteUnit): string {
  if (!Number.isSafeInteger(bytes) || bytes < 1 || BigInt(bytes) > MAX_BYTES) {
    throw new Error("bytes out of range");
  }
  return exactDecimal(BigInt(bytes), BYTE_FACTORS[unit]);
}

/** 表单可选字节单位：value 为配置契约字面量，label 使用 B/KB/MB/GB/TB 标签。 */
export const BYTE_UNIT_OPTIONS: { value: ByteUnit; label: string }[] = [
  { value: "B", label: "B" },
  { value: "KB", label: "KB" },
  { value: "MB", label: "MB" },
  { value: "GB", label: "GB" },
  { value: "TB", label: "TB" },
];

/** 回显的单位阶梯，从大到小；MAX_BYTES 恰好是 1 TiB，再大没有可表示的单位。 */
const BYTE_DISPLAY_UNITS: readonly ByteUnit[] = ["TB", "GB", "MB", "KB", "B"];

/** 回显数值允许的小数位上限：更细的分辨率对配置字段没有意义，只会让用户看到长尾小数。 */
const DISPLAY_SCALE = 100n;

/**
 * 接口返回的字节数换算成「数值 + 单位」：从大到小挑选能精确表示且小数不超过两位的最大单位，
 * 保证回显无损——用户不动该字段时再次保存不会把字节数改掉；B 是所有单位的公约数，必定命中。
 * 非安全整数或超出 schema 上限时抛错，由调用方决定提示方式。
 */
export function bytesToDisplay(bytes: number): { value: string; unit: ByteUnit } {
  if (!Number.isSafeInteger(bytes) || bytes < 1 || BigInt(bytes) > MAX_BYTES) {
    throw new Error("bytes out of range");
  }
  const scaled = BigInt(bytes) * DISPLAY_SCALE;
  // 乘 100 后能被换算因子整除，等价于该单位下的数值精确且最多两位小数。
  const unit = BYTE_DISPLAY_UNITS.find((candidate) => scaled % BYTE_FACTORS[candidate] === 0n) ?? "B";
  return { value: bytesToForm(bytes, unit), unit };
}

/** 解析紧凑复合 duration 为纳秒；不在前端复制各业务字段的上下界。 */
export function durationToNanoseconds(value: string): bigint {
  if (!value || value.length > 128 || value.trim() !== value) throw new Error("invalid duration");
  const token = /(\d+(?:\.\d+)?)(ns|us|ms|s|m|h|d|w)/gy;
  let total = 0n;
  let offset = 0;
  for (let match = token.exec(value); match; match = token.exec(value)) {
    if (match.index !== offset) throw new Error("invalid duration");
    total += parseExactDecimal(match[1], DURATION_FACTORS[match[2] as keyof typeof DURATION_FACTORS]);
    offset = token.lastIndex;
  }
  if (offset !== value.length) throw new Error("invalid duration");
  return total;
}

export function durationFromNanoseconds(value: bigint): string {
  if (value < 0n) throw new Error("invalid duration");
  if (value === 0n) return "0ns";
  let remaining = value;
  const parts: string[] = [];
  for (const [unit, factor] of Object.entries(DURATION_FACTORS) as [keyof typeof DURATION_FACTORS, bigint][]) {
    const count = remaining / factor;
    if (count > 0n) {
      parts.push(`${count}${unit}`);
      remaining %= factor;
    }
  }
  return parts.join("");
}
/** 时长可选单位：只覆盖人可感知的量级，刻意不提供 ns/us，避免暴露不可感知的纳秒值。 */
export const DURATION_UNITS = ["ms", "s", "m", "h", "d"] as const;

export type DurationUnit = (typeof DURATION_UNITS)[number];

/** 空值与新建记录的默认单位固定为秒。 */
export const DEFAULT_DURATION_UNIT: DurationUnit = "s";

export const DURATION_UNIT_LABELS: Record<DurationUnit, string> = {
  ms: "毫秒",
  s: "秒",
  m: "分钟",
  h: "小时",
  d: "天",
};

/** 回显时从大到小挑选可整除的单位，天数等大单位不会退化成 86400 秒这样的数字。 */
const WHOLE_DURATION_UNITS: readonly DurationUnit[] = ["d", "h", "m", "s"];

export interface DurationFormValue {
  /** 十进制数值字符串，保留用户输入精度，不经过浮点数往返。 */
  amount: string;
  unit: DurationUnit;
}

/**
 * 接口返回的紧凑 duration（后端恒为纳秒串）换算成「数值 + 单位」：
 * 优先用能整除的最大单位，不足一秒才按毫秒精确展开。语法非法时抛错，由调用方决定提示方式。
 */
export function durationToForm(value: string | undefined): DurationFormValue {
  if (value === undefined || value === "") return { amount: "", unit: DEFAULT_DURATION_UNIT };
  const nanos = durationToNanoseconds(value);
  if (nanos === 0n) return { amount: "0", unit: DEFAULT_DURATION_UNIT };
  for (const unit of WHOLE_DURATION_UNITS) {
    const factor = DURATION_FACTORS[unit];
    if (nanos % factor === 0n) return { amount: exactDecimal(nanos, factor), unit };
  }
  return { amount: exactDecimal(nanos, DURATION_FACTORS.ms), unit: "ms" };
}

/**
 * 摘要与表格展示用文本：把纳秒串显示成最大整单位（`300000000000ns` → `5 分钟`）。
 * 语法非法时原样返回，避免展示层静默改写后端事实。
 */
export function formatDurationText(value: string | undefined): string {
  if (value === undefined || value === "") return "";
  try {
    const form = durationToForm(value);
    return form.amount === "" ? value : `${form.amount} ${DURATION_UNIT_LABELS[form.unit]}`;
  } catch {
    return value;
  }
}

/**
 * 「数值 + 单位」组合回紧凑 duration，只校验精确可表示性与语法；
 * 正数、上下界等字段语义仍由后端权威校验。
 */
export function durationFromForm(amount: string, unit: DurationUnit): string {
  const trimmed = amount.trim();
  if (trimmed === "" || trimmed.length > 64) throw new Error("invalid duration");
  const factor = DURATION_FACTORS[unit];
  const decimal = exactDecimal(parseExactDecimal(trimmed, factor), factor);
  // 后端 parse_duration 只接受 1..9 位小数，超出时抛错让控件保留原文并由表单校验提示。
  if (/\.\d{10,}$/.test(decimal)) throw new Error("invalid duration");
  return `${decimal}${unit}`;
}

/**
 * 把接口的纳秒串归一化成与表单等价的紧凑串（5000000000ns → 5s），
 * 用于表单回填：用户未编辑该字段时也不会把纳秒量级写回变更报文。
 * 语法非法时原样返回，交由控件与校验处理。
 */
export function normalizeDuration(value: string | undefined): string | undefined {
  if (value === undefined) return undefined;
  try {
    const form = durationToForm(value);
    return form.amount === "" ? value : durationFromForm(form.amount, form.unit);
  } catch {
    return value;
  }
}
/** 供表单校验使用：语法合法且严格大于 0（后端拒绝零值超时）。 */
export function isPositiveDuration(value: string | undefined): boolean {
  if (!value) return false;
  try {
    return durationToNanoseconds(value) > 0n;
  } catch {
    return false;
  }
}

/**
 * 供表单校验使用：语法合法且允许 0。
 * TTL 上下限的 `0s` 在后端与配置文档中表示“该边界不设限”，不能当非法值拒绝。
 */
export function isNonNegativeDuration(value: string | undefined): boolean {
  if (!value) return false;
  try {
    return durationToNanoseconds(value) >= 0n;
  } catch {
    return false;
  }
}

/** 只负责 IP/CIDR 词法检查；业务冲突与网段归一化仍由后端权威校验。 */
export function isIpOrCidr(value: string): boolean {
  if (!value || value.length > 64 || value.trim() !== value) return false;
  const parts = value.split("/");
  if (parts.length > 2) return false;
  const address = parts[0];
  const version = isIpv4(address) ? 4 : isIpv6(address) ? 6 : 0;
  if (version === 0) return false;
  if (parts.length === 1) return true;
  if (!/^(0|[1-9][0-9]{0,2})$/.test(parts[1])) return false;
  const prefix = Number(parts[1]);
  return prefix <= (version === 4 ? 32 : 128);
}

export function deserializeInheritance<T>(value: T | undefined): Inheritable<T> {
  return value === undefined ? { kind: "inherit" } : { kind: "value", value };
}

export function serializeInheritance<T>(value: Inheritable<T>): T | undefined {
  return value.kind === "inherit" ? undefined : value.value;
}

/** 当前引用已被删除时仍展示禁用项，避免选择器静默清空编辑快照。 */
export function referenceOptions(values: readonly string[], selected?: string): ReferenceOption[] {
  const unique = [...new Set(values)].sort((left, right) => left.localeCompare(right));
  const options: ReferenceOption[] = unique.map((value) => ({ value, label: value }));
  if (selected && !unique.includes(selected)) {
    options.unshift({ value: selected, label: `${selected}（已不存在）`, disabled: true, missing: true });
  }
  return options;
}

/** variant 切换后只选择白名单字段，隐藏分支字段不会残留到提交 payload。 */
export function selectVariantPayload<T extends { type: string }, K extends keyof T>(
  source: T,
  type: T["type"],
  fields: readonly K[],
): Pick<T, K> {
  if (source.type !== type) throw new Error("variant type mismatch");
  return Object.fromEntries(fields.filter((field) => source[field] !== undefined).map((field) => [field, source[field]])) as Pick<T, K>;
}

function parseExactDecimal(value: string, factor: bigint): bigint {
  const match = /^(0|[1-9][0-9]*)(?:\.([0-9]+))?$/.exec(value);
  if (!match) throw new Error("invalid decimal");
  const fraction = match[2] ?? "";
  const scale = 10n ** BigInt(fraction.length);
  const numerator = BigInt(match[1]) * scale + BigInt(fraction || "0");
  const scaled = numerator * factor;
  if (scaled % scale !== 0n) throw new Error("value cannot be represented exactly");
  return scaled / scale;
}

function exactDecimal(value: bigint, factor: bigint): string {
  const integer = value / factor;
  let remainder = value % factor;
  if (remainder === 0n) return integer.toString();
  let fraction = "";
  while (remainder !== 0n) {
    remainder *= 10n;
    fraction += (remainder / factor).toString();
    remainder %= factor;
  }
  return `${integer}.${fraction}`;
}

function isIpv4(value: string): boolean {
  const octets = value.split(".");
  return octets.length === 4 && octets.every((octet) =>
    /^(0|[1-9][0-9]{0,2})$/.test(octet) && Number(octet) <= 255,
  );
}

function isIpv6(value: string): boolean {
  if (!value.includes(":")) return false;
  try {
    const parsed = new URL(`http://[${value}]/`);
    return parsed.hostname.startsWith("[") && parsed.hostname.endsWith("]");
  } catch {
    return false;
  }
}
