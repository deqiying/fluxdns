import { InputNumber, Select } from "antd";
import { useEffect, useRef, useState } from "react";
import {
  DEFAULT_DURATION_UNIT,
  DURATION_UNIT_LABELS,
  DURATION_UNITS,
  durationFromForm,
  durationToForm,
  isNonNegativeDuration,
  isPositiveDuration,
  type DurationFormValue,
  type DurationUnit,
} from "@/shared/config/form-values";

const unitOptions = DURATION_UNITS.map((unit) => ({ label: DURATION_UNIT_LABELS[unit], value: unit }));

export interface DurationInputProps {
  /** 紧凑 duration 字符串；接口回显恒为纳秒串，提交仍写回紧凑串。 */
  value?: string;
  onChange?: (value: string | undefined) => void;
  disabled?: boolean;
  /** antd Form 注入的字段 id，用于把 Form.Item label 关联到数字输入。 */
  id?: string;
  /** 无障碍名称，例如「主要超时」。 */
  label: string;
}

/** 超时等字段的必填 + 正数校验：零值会被后端拒绝，前端提前给出同义提示。 */
export const durationRequiredRules = [
  { required: true, message: "请填写时长" },
  {
    validator: async (_: unknown, value: string | undefined) => {
      if (!isPositiveDuration(value)) throw new Error("请填写大于 0 的有效时长");
    },
  },
];

/**
 * 可选 duration 字段（如 TTL 上下限）：留空表示不设置，
 * 填写时允许 0——TTL 的 `0s` 表示“该边界不设限”，只有非法串被拒绝。
 */
export const durationOptionalRules = [
  {
    validator: async (_: unknown, value: string | undefined) => {
      if (value === undefined || value === "") return;
      if (!isNonNegativeDuration(value)) throw new Error("请填写大于等于 0 的有效时长");
    },
  },
];

/**
 * 时长输入：数字 + 单位下拉，默认单位为秒。
 * 纳秒只在回显时换算一次，用户看到与输入的都是秒/毫秒等可感知量级，
 * 提交值仍是后端 Duration 契约的紧凑串（如 `5s`、`1500ms`）。
 */
export function DurationInput({ id, value, onChange, disabled, label }: DurationInputProps) {
  const [fields, setFields] = useState<DurationFormValue>(() => toFields(value));
  // 记录本控件最近一次写出的值：表单回显同一字符串时不重置用户正在输入的单位与数值。
  const emitted = useRef<string | undefined>(value);
  useEffect(() => {
    if (value === emitted.current) return;
    emitted.current = value;
    setFields(toFields(value));
  }, [value]);

  const change = (next: DurationFormValue) => {
    setFields(next);
    const composed = compose(next);
    emitted.current = composed;
    onChange?.(composed);
  };

  return (
    <div className="duration-input">
      <InputNumber
        stringMode
        id={id}
        style={{ width: "100%" }}
        aria-label={label}
        disabled={disabled}
        placeholder="5"
        value={fields.amount === "" ? null : fields.amount}
        onChange={(next: unknown) => change({ ...fields, amount: next === null || next === undefined ? "" : String(next) })}
      />
      <Select
        aria-label={`${label}单位`}
        disabled={disabled}
        options={unitOptions}
        value={fields.unit}
        onChange={(unit: DurationUnit) => change({ ...fields, unit })}
      />
    </div>
  );
}

function toFields(value: string | undefined): DurationFormValue {
  try {
    return durationToForm(value);
  } catch {
    // 语法非法的原始值按空值回显，交给必填/正数校验提示重填，避免静默改写用户配置。
    return { amount: "", unit: DEFAULT_DURATION_UNIT };
  }
}

function compose(fields: DurationFormValue): string | undefined {
  if (fields.amount.trim() === "") return undefined;
  try {
    return durationFromForm(fields.amount, fields.unit);
  } catch {
    // 无法精确表示时保留原文，让表单校验给出提示而不是悄悄改数。
    return `${fields.amount}${fields.unit}`;
  }
}
