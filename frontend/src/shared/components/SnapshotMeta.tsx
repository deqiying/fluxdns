import { Space, Typography } from "antd";
import { formatDateTime } from "@/shared/formatters";

export function SnapshotMeta({ sampledAt, revision }: { sampledAt?: string; revision?: string }) {
  return (
    <Space size="middle" wrap>
      {sampledAt ? <Typography.Text type="secondary">采样：{formatDateTime(sampledAt)}</Typography.Text> : null}
      {revision ? <Typography.Text type="secondary">Runtime：{revision}</Typography.Text> : null}
    </Space>
  );
}
