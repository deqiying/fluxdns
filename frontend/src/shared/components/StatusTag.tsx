import { Badge, Tag } from "antd";
import type { HealthStatus } from "@/shared/api/types";

const healthStatusMeta: Record<HealthStatus, { color: string; label: string }> = {
  healthy: { color: "success", label: "健康" },
  degraded: { color: "warning", label: "降级" },
  failed: { color: "error", label: "故障" },
  stopping: { color: "default", label: "停止中" },
};

export function HealthStatusTag({ status }: { status: HealthStatus }) {
  const meta = healthStatusMeta[status];
  return <Tag color={meta.color}>{meta.label}</Tag>;
}

export function BooleanStatus({ value, trueLabel, falseLabel }: { value: boolean; trueLabel: string; falseLabel: string }) {
  return <Badge status={value ? "success" : "default"} text={value ? trueLabel : falseLabel} />;
}
