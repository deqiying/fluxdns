export type ByteUnit = "B" | "MiB" | "GiB";

const BYTE_FACTORS: Record<ByteUnit, bigint> = {
  B: 1n,
  MiB: 1_048_576n,
  GiB: 1_073_741_824n,
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
