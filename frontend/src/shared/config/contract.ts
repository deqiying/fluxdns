import type { components } from "@/shared/api/generated-v2";

type Schemas = components["schemas"];
export type ConfigChange = Schemas["ConfigChange"];
export type ConfigState = Schemas["ConfigState"];
export type ConfigDraft = {
  /** 打开草稿时固定，refetch 和外部变化不能替换它。 */
  expected: Schemas["Preconditions"];
  change: ConfigChange;
  dirty: boolean;
};

export type FormPhase =
  | { kind: "editing"; draft: ConfigDraft }
  | { kind: "validating"; draft: ConfigDraft }
  | { kind: "confirming"; draft: ConfigDraft; validation: Schemas["ValidationResult"] }
  | { kind: "applying"; draft: ConfigDraft; operationId: string }
  | { kind: "result_unknown"; draft: ConfigDraft; operationId: string }
  | { kind: "settled"; draft: ConfigDraft; operation: Schemas["OperationResult"] };

/** 只选普通编辑字段；ID 只用于创建，不能随名称修改或外部差异一起提交。 */
export function clientEditValue(source: Schemas["Client"]): Schemas["ClientEdit"] {
  const { name, match, strategy, cache, ttl_override, edns_client_subnet } = source;
  return { name, match, strategy, cache, ttl_override, edns_client_subnet };
}

/** HTTP 200 不代表文件已同步；unknown 只能触发回读，不能转成可自动重试。 */
export function operationDisposition(status: Schemas["OperationStatus"]) {
  switch (status.state) {
    case "preparing":
    case "applying":
    case "persisting":
      return "poll";
    case "applied_synced":
      return "synchronized";
    case "applied_unpersisted":
      return "retry_persistence";
    case "rejected":
      return "keep_draft";
    case "compensation_failed":
      return "blocked";
    case "unknown":
      return "read_active_state";
  }
}

/** 大整数保留十进制字符串；只在确认目标范围可安全表示时转换给数值表单。 */
export function decimalToSafeInteger(value: string): number {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) throw new Error("invalid decimal integer");
  const integer = BigInt(value);
  if (integer > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error("integer exceeds safe form range");
  return Number(integer);
}
