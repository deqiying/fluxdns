import { Alert, App, Button, Descriptions, Drawer, Empty, List, Space, Spin, Tag, Typography } from "antd";
import { GitMerge, RotateCcw } from "lucide-react";
import type { ExternalWorkspaceState } from "@/shared/config/external-change";
import { getSafeErrorMessage } from "@/shared/api/errors";

export interface ExternalChangeDrawerProps {
  state: ExternalWorkspaceState;
  onClose: () => void;
  onRestore: () => void;
  onRetryPersistence?: () => void;
  onAdopt?: () => void;
}

export function ExternalChangeDrawer({
  state,
  onClose,
  onRestore,
  onRetryPersistence,
  onAdopt,
}: ExternalChangeDrawerProps) {
  const { modal } = App.useApp();
  const busy = state.phase === "loading" || state.phase === "restoring";
  const requestClose = () => {
    if (busy) return;
    if (!state.dirty) {
      onClose();
      return;
    }
    modal.confirm({
      title: "放弃差异草稿？",
      content: "尚未采用的修改将被丢弃。",
      okText: "放弃修改",
      cancelText: "继续编辑",
      okButtonProps: { danger: true },
      onOk: onClose,
    });
  };
  const confirmRestore = () => modal.confirm({
    title: "还原受管配置文件？",
    content: "将以当前活动配置覆盖已观察到的文件版本，不会回滚正在运行的 DNS 配置。",
    okText: "还原文件",
    cancelText: "取消",
    onOk: onRestore,
  });

  return (
    <Drawer
      className="external-change-drawer"
      open={state.open}
      title="配置文件变化"
      width={640}
      destroyOnHidden
      keyboard={!busy}
      mask={{ closable: !busy }}
      onClose={requestClose}
      footer={state.issue ? (
        <Space wrap>
          <Button
            icon={<RotateCcw size={16} aria-hidden="true" />}
            disabled={busy}
            onClick={confirmRestore}
          >
            还原文件
          </Button>
          {state.issue.kind === "applied_unpersisted" && onRetryPersistence ? (
            <Button type="primary" loading={state.phase === "restoring"} onClick={onRetryPersistence}>
              重试文件同步
            </Button>
          ) : null}
          {onAdopt ? (
            <Button type="primary" icon={<GitMerge size={16} aria-hidden="true" />} disabled={busy} onClick={onAdopt}>
              修改并采用
            </Button>
          ) : null}
        </Space>
      ) : null}
    >
      <ExternalChangeDrawerBody state={state} />
    </Drawer>
  );
}

function ExternalChangeDrawerBody({ state }: { state: ExternalWorkspaceState }) {
  if (state.phase === "loading") return <div className="external-change-loading"><Spin /></div>;
  if (state.phase === "error") return <Alert type="error" showIcon message={getSafeErrorMessage(state.error)} />;
  if (state.phase === "conflict") {
    return <Alert type="warning" showIcon message="配置文件已再次变化，请重新读取差异。" />;
  }
  if (!state.diff) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="暂无可显示的差异" />;

  const { diff } = state;
  return (
    <Space direction="vertical" size={20} className="external-change-content">
      <Descriptions size="small" column={1} bordered>
        <Descriptions.Item label="活动版本">{diff.expected.active_revision}</Descriptions.Item>
        <Descriptions.Item label="文件版本">{diff.expected.observed_file_revision}</Descriptions.Item>
      </Descriptions>
      {diff.parse_error ? <Alert type="error" showIcon message={`文件无法解析：${diff.parse_error}`} /> : null}
      {diff.protected_changes.length > 0 ? (
        <div>
          <Typography.Title level={5}>受保护变化</Typography.Title>
          <Space wrap>{diff.protected_changes.map((change) => <Tag key={change}>{change}</Tag>)}</Space>
        </div>
      ) : null}
      <div>
        <Typography.Title level={5}>可编辑变化</Typography.Title>
        <List
          size="small"
          bordered
          locale={{ emptyText: "无可编辑变化" }}
          dataSource={diff.editable}
          renderItem={(item, index) => (
            <List.Item>
              <Typography.Text>{item.external?.module ?? item.active?.module ?? `变化 ${index + 1}`}</Typography.Text>
            </List.Item>
          )}
        />
      </div>
    </Space>
  );
}
