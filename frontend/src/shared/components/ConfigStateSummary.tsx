import { Space, Tag, Typography } from "antd";
import type { ConfigState } from "@/shared/config/api";

const syncLabels: Record<ConfigState["synchronization"], string> = {
  synced: "已同步",
  applying: "应用中",
  persisting: "同步文件中",
  applied_unpersisted: "文件未同步",
  blocked: "配置已阻塞",
};

export function ConfigStateSummary({ state }: { state: ConfigState }) {
  const color = state.synchronization === "synced"
    ? "success"
    : state.synchronization === "blocked"
      ? "error"
      : "warning";
  return (
    <Space wrap size={8} className="config-state-summary">
      <Tag color={color}>{syncLabels[state.synchronization]}</Tag>
      <Typography.Text type="secondary">活动版本 {state.active_revision}</Typography.Text>
      <Typography.Text type="secondary">文件版本 {state.observed_file_revision}</Typography.Text>
    </Space>
  );
}
