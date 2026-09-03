import { Card, Col, Row, Space, Statistic, Table, Tag, Typography, type TableColumnsType } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { SnapshotMeta } from "@/shared/components/SnapshotMeta";
import { BooleanStatus } from "@/shared/components/StatusTag";
import type { BindEntry } from "@/shared/api/types";
import { useRuntime } from "./hooks";

const columns: TableColumnsType<BindEntry> = [
  {
    title: "协议",
    dataIndex: "transport",
    width: 100,
    render: (value: BindEntry["transport"]) => <Tag color="cyan">{value.toUpperCase()}</Tag>,
  },
  {
    title: "监听地址",
    key: "endpoint",
    render: (_, entry) => <Typography.Text code>{`${entry.address}:${entry.port}`}</Typography.Text>,
  },
  { title: "Owner", dataIndex: "owner" },
  {
    title: "IPv6 only",
    dataIndex: "v6_only",
    width: 120,
    render: (value: boolean) => (value ? "是" : "否"),
  },
  {
    title: "状态",
    dataIndex: "state",
    width: 110,
    render: (value: BindEntry["state"]) => (
      <Tag color={value === "active" ? "success" : "warning"}>{value === "active" ? "运行中" : "排空中"}</Tag>
    ),
  },
];

export function RuntimePage() {
  const query = useRuntime();
  const runtime = query.data;

  return (
    <PageFrame
      title="Runtime"
      description="查看当前 revision、策略装配和 listener/bind 摘要；本页不提供 reload 或 restart。"
      meta={runtime ? <SnapshotMeta sampledAt={runtime.sampled_at} revision={runtime.revision} /> : undefined}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {runtime ? (
        <Space orientation="vertical" size={20} style={{ width: "100%" }}>
          <Row gutter={[16, 16]}>
            <Col xs={12} lg={6}>
              <Card className="snapshot-card"><Statistic title="Listeners" value={runtime.listener_count} /></Card>
            </Col>
            <Col xs={12} lg={6}>
              <Card className="snapshot-card"><Statistic title="Binds" value={runtime.bind_count} /></Card>
            </Col>
            <Col xs={12} lg={6}>
              <Card className="snapshot-card"><Statistic title="Resources" value={runtime.resource_count} /></Card>
            </Col>
            <Col xs={12} lg={6}>
              <Card className="snapshot-card">
                <Statistic
                  title="Policy Core"
                  valueRender={() => (
                    <BooleanStatus value={runtime.has_policy_core} trueLabel="已装配" falseLabel="未装配" />
                  )}
                />
              </Card>
            </Col>
          </Row>
          <Card className="data-card" title="监听与绑定">
            <PageState empty={runtime.binds.length === 0} emptyDescription="当前 Runtime 没有活动 bind" />
            {runtime.binds.length > 0 ? (
              <Table rowKey={(entry) => `${entry.transport}:${entry.address}:${entry.port}:${entry.owner}`} columns={columns} dataSource={runtime.binds} pagination={false} scroll={{ x: 760 }} />
            ) : null}
          </Card>
          <Typography.Text type="secondary">配置摘要：{runtime.normalized_hash}</Typography.Text>
        </Space>
      ) : null}
    </PageFrame>
  );
}
