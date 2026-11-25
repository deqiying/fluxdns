import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];

/** 只选普通编辑字段且不带 ID；修改请求身份必须由调用方显式追加 client_id。 */
export function clientEditValue(source: Schemas["Client"]): Schemas["ClientEdit"] {
  const { name, match, strategy, cache, ttl_override, edns_client_subnet } = source;
  return { name, match, strategy, cache, ttl_override, edns_client_subnet };
}

/** 大整数保留十进制字符串；只在确认目标范围可安全表示时转换给数值表单。 */
export function decimalToSafeInteger(value: string): number {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) throw new Error("invalid decimal integer");
  const integer = BigInt(value);
  if (integer > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error("integer exceeds safe form range");
  return Number(integer);
}
