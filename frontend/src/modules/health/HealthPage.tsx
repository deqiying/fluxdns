import { Card, Table, Tag, Typography, type TableColumnsType } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { SnapshotMeta } from "@/shared/components/SnapshotMeta";
import { HealthStatusTag } from "@/shared/components/StatusTag";
import { formatDateTime } from "@/shared/formatters";
import type { ComponentHealth } from "@/shared/api/types";
import { useHealth } from "./hooks";

const columns: TableColumnsType<ComponentHealth> = [
  { title: "组件", dataIndex: "component", fixed: "left", width: 180 },
  { title: "状态", dataIndex: "status", width: 110, render: (value) => <HealthStatusTag status={value} /> },
  { title: "原因码", dataIndex: "reason_code", render: (value: string) => <Typography.Text code>{value}</Typography.Text> },
  { title: "最近变化", dataIndex: "last_changed_at", width: 210, render: formatDateTime },
  { title: "最近成功", dataIndex: "last_success_at", width: 210, render: formatDateTime },
  { title: "重试", dataIndex: "retry_count", width: 80 },
  {
    title: "标记",
    key: "flags",
    width: 150,
    render: (_, item) => (
      <>
        {item.stale ? <Tag color="warning">STALE</Tag> : null}
        {item.gap ? <Tag color="error">GAP</Tag> : null}
        {!item.stale && !item.gap ? <Tag>正常</Tag> : null}
      </>
    ),
  },
];

export function HealthPage() {
  const query = useHealth();
  const health = query.data;

  return (
    <PageFrame
      title="健康状态"
      description="展示服务端稳定状态和安全原因码；请求失败与组件 degraded/failed 分别表达。"
      meta={health ? <SnapshotMeta sampledAt={health.sampled_at} /> : undefined}
      actions={health ? <HealthStatusTag status={health.overall_status} /> : undefined}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {health ? (
        <Card className="data-card">
          <PageState empty={health.components.length === 0} emptyDescription="服务端尚未报告组件状态" />
          {health.components.length > 0 ? (
            <Table rowKey="component" columns={columns} dataSource={health.components} pagination={false} scroll={{ x: 980 }} />
          ) : null}
        </Card>
      ) : null}
    </PageFrame>
  );
}
