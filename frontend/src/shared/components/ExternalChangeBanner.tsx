import { Alert, Button, Space } from "antd";
import { PanelTopOpen, X } from "lucide-react";
import type { ExternalIssue } from "@/shared/config/external-change";

export interface ExternalChangeBannerProps {
  issue: ExternalIssue;
  onOpen: () => void;
  onDismiss: () => void;
}

const issueMessages: Record<ExternalIssue["kind"], string> = {
  files_changed: "配置文件已在外部修改",
  files_missing: "受管配置文件缺失",
  files_unreadable: "受管配置文件无法读取",
  files_oversized: "受管配置文件超过读取上限",
  applied_unpersisted: "运行配置已生效，但文件尚未同步",
  blocked: "配置同步已阻塞",
};

export function ExternalChangeBanner({ issue, onOpen, onDismiss }: ExternalChangeBannerProps) {
  return (
    <Alert
      className="external-change-banner"
      type={issue.kind === "blocked" ? "error" : "warning"}
      showIcon
      message={issueMessages[issue.kind]}
      action={(
        <Space size={4}>
          <Button type="link" size="small" icon={<PanelTopOpen size={15} aria-hidden="true" />} onClick={onOpen}>
            查看
          </Button>
          <Button
            type="text"
            size="small"
            icon={<X size={15} aria-hidden="true" />}
            aria-label="关闭提示"
            onClick={onDismiss}
          />
        </Space>
      )}
    />
  );
}
