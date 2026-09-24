import { InputNumber, Select } from "antd";
import { useEffect, useRef, useState } from "react";
import {
  BYTE_UNIT_OPTIONS,
  bytesFromForm,
  bytesToDisplay,
  type ByteUnit,
} from "@/shared/config/form-values";

/** 空值与非法回显的兜底单位：B 不会把用户输入隐式放大一个量级。 */
const DEFAULT_BYTE_UNIT: ByteUnit = "B";

interface ByteSizeFields {
  /** 十进制数值字符串，保留用户输入精度，不经过浮点数往返。 */
  amount: string;
  unit: ByteUnit;
}

export interface ByteSizeInputProps {
  /** 字节数；undefined 表示当前输入无法换算成合法字节数（由 Form.Item 的 required 规则拦截）。 */
  value?: number;
  onChange?: (bytes: number | undefined) => void;
  disabled?: boolean;
  /** antd Form 注入的字段 id，用于把 Form.Item label 关联到数字输入。 */
  id?: string;
  /** 无障碍名称，例如「内存上限」。 */
  label: string;
}

/**
 * 字节大小输入：数字 + 单位下拉（B/KB/MB/GB/TB，1024 进制）。
 * 回显按能精确表示的最大单位拆分；用户改动数值或单位后换算回整数字节提交。
 * 无法换算（空值、非法小数、超出 schema 上限）时对外 emit undefined，
 * 由 Form.Item 的 required 规则报错——绝不静默保留上一次有效值，否则界面与提交值会量级不一致。
 */
export function ByteSizeInput({ id, value, onChange, disabled, label }: ByteSizeInputProps) {
  const [fields, setFields] = useState<ByteSizeFields>(() => toFields(value));
  // 记录本控件最近一次写出的值：表单回显同一值（含 undefined）时不再重置用户正在输入的数值与单位。
  const emitted = useRef<number | undefined>(value);
  useEffect(() => {
    if (value === emitted.current) return;
    emitted.current = value;
    setFields(toFields(value));
  }, [value]);

  const change = (next: ByteSizeFields) => {
    setFields(next);
    const amount = next.amount.trim();
    if (amount === "") {
      // 清空输入等价于未填写：保留旧值会让 required 规则误判通过。
      emitted.current = undefined;
      onChange?.(undefined);
      return;
    }
    try {
      const bytes = bytesFromForm(amount, next.unit);
      emitted.current = bytes;
      onChange?.(bytes);
    } catch {
      // 换算失败同样置为 undefined：控件保留用户原文，失败由 required 规则显式报错。
      emitted.current = undefined;
      onChange?.(undefined);
    }
  };

  return (
    <div className="byte-size-input">
      <InputNumber
        stringMode
        id={id}
        style={{ width: "100%" }}
        aria-label={label}
        disabled={disabled}
        placeholder="64"
        value={fields.amount === "" ? null : fields.amount}
        onChange={(next: unknown) => change({ ...fields, amount: next === null || next === undefined ? "" : String(next) })}
      />
      <Select
        aria-label={`${label}单位`}
        disabled={disabled}
        options={BYTE_UNIT_OPTIONS}
        value={fields.unit}
        onChange={(unit: ByteUnit) => change({ ...fields, unit })}
      />
    </div>
  );
}

function toFields(bytes: number | undefined): ByteSizeFields {
  if (bytes === undefined) return { amount: "", unit: DEFAULT_BYTE_UNIT };
  try {
    const display = bytesToDisplay(bytes);
    return { amount: display.value, unit: display.unit };
  } catch {
    // 越界或非整数的原始值按空值回显，交给必填校验提示重填，避免静默改写用户配置。
    return { amount: "", unit: DEFAULT_BYTE_UNIT };
  }
}
