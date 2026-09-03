import { Card, Col, Row, Space, Typography } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState, InlineUnavailable } from "@/shared/components/PageState";
import { SnapshotMeta } from "@/shared/components/SnapshotMeta";
import { HealthStatusTag } from "@/shared/components/StatusTag";
import { formatCount, formatPercent } from "@/shared/formatters";
import type { OverviewCard } from "@/shared/api/types";
import { useOverview } from "./hooks";

function formatMetric(card: OverviewCard): string {
  if (card.value === undefined || card.value === null) return "—";
  return card.unit === "percent" ? formatPercent(card.value) : formatCount(card.value);
}

export function DashboardPage() {
  const query = useOverview();
  const overview = query.data;

  return (
    <PageFrame
      title="运行总览"
      description="聚合展示服务端生成的有界指标；各卡片可独立标记不可用。"
      meta={overview ? <SnapshotMeta sampledAt={overview.sampled_at} revision={overview.runtime_revision} /> : undefined}
      actions={overview ? <HealthStatusTag status={overview.overall_status} /> : undefined}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {overview ? (
        <Space orientation="vertical" size={22} style={{ width: "100%" }}>
          <Row gutter={[18, 18]}>
            {overview.cards.map((card) => (
              <Col xs={24} sm={12} xl={8} xxl={6} key={card.key}>
                <Card className="metric-card">
                  <div className="metric-label">{card.label}</div>
                  {card.status === "available" ? (
                    <div className="metric-value">{formatMetric(card)}</div>
                  ) : (
                    <div style={{ marginTop: 18 }}>
                      <InlineUnavailable reasonCode={card.unavailable_reason_code} />
                    </div>
                  )}
                </Card>
              </Col>
            ))}
          </Row>
          <Card className="snapshot-card">
            <Space orientation="vertical" size={6}>
              <Typography.Text strong>快照边界</Typography.Text>
              <Typography.Text type="secondary">
                当前页面只展示采样时刻的服务端摘要，不将其他页面的不同采样响应合并为同一快照。
              </Typography.Text>
            </Space>
          </Card>
        </Space>
      ) : null}
    </PageFrame>
  );
}
