import { Card, Descriptions, Tag } from "antd";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { formatDateTime, formatUptime } from "@/shared/formatters";
import { useSystem } from "./hooks";

const capabilityLabels: Record<string, string> = {
  "read:overview": "总览",
  "read:runtime": "Runtime",
  "read:health": "健康状态",
  "read:statistics": "统计",
  "read:queries": "解析记录",
  "read:resources": "资源",
  "read:system": "系统",
};

export function SystemPage() {
  const query = useSystem();
  const system = query.data;

  return (
    <PageFrame title="系统信息" description="仅显示服务版本、运行时间和服务端显式声明的只读能力。">
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {system ? (
        <Card className="data-card">
          <Descriptions bordered column={{ xs: 1, sm: 1, md: 2 }}>
            <Descriptions.Item label="FluxDNS 版本">{system.version || "未知"}</Descriptions.Item>
            <Descriptions.Item label="启动时间">{formatDateTime(system.started_at)}</Descriptions.Item>
            <Descriptions.Item label="运行时长">{formatUptime(system.uptime_seconds)}</Descriptions.Item>
            <Descriptions.Item label="能力">
              {system.capabilities.length > 0
                ? system.capabilities.map((capability) => (
                    <Tag key={capability} color="cyan">{capabilityLabels[capability] ?? capability}</Tag>
                  ))
                : "未声明"}
            </Descriptions.Item>
          </Descriptions>
        </Card>
      ) : null}
    </PageFrame>
  );
}
