import { Card, Table, Tag, Typography, type TableColumnsType } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { SnapshotMeta } from "@/shared/components/SnapshotMeta";
import type { ResourceSummary } from "@/shared/api/types";
import { useResources } from "./hooks";

const columns: TableColumnsType<ResourceSummary> = [
  { title: "资源", dataIndex: "display_name", fixed: "left", width: 220 },
  { title: "类型", dataIndex: "source_kind", width: 110, render: (value: string) => <Tag>{value.toUpperCase()}</Tag> },
  { title: "Epoch", dataIndex: "epoch", width: 150, render: (value: string) => <Typography.Text code>{value}</Typography.Text> },
  { title: "Revision", dataIndex: "revision", width: 170, render: (value: string) => <Typography.Text code>{value}</Typography.Text> },
  { title: "Fallback", dataIndex: "fallback", width: 110, render: (value: boolean) => <Tag color={value ? "warning" : "success"}>{value ? "是" : "否"}</Tag> },
  { title: "Stale", dataIndex: "stale", width: 100, render: (value: boolean) => <Tag color={value ? "error" : "success"}>{value ? "是" : "否"}</Tag> },
];

export function ResourcesPage() {
  const query = useResources();
  const resources = query.data;

  return (
    <PageFrame
      title="资源状态"
      description="只展示资源版本、来源类型、fallback 和 stale 元数据；不下载规则正文，也不提供刷新动作。"
      meta={resources ? <SnapshotMeta sampledAt={resources.sampled_at} revision={resources.runtime_revision} /> : undefined}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {resources ? (
        <Card className="data-card">
          <PageState empty={resources.items.length === 0} emptyDescription="当前 Runtime 没有资源摘要" />
          {resources.items.length > 0 ? (
            <Table rowKey="id" columns={columns} dataSource={resources.items} pagination={false} scroll={{ x: 900 }} />
          ) : null}
        </Card>
      ) : null}
    </PageFrame>
  );
}
