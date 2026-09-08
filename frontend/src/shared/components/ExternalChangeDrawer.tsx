import { useEffect, useMemo, useState } from "react";
import { Alert, App, Button, Checkbox, Descriptions, Drawer, Empty, List, Space, Spin, Tag, Typography } from "antd";
import { GitMerge, RotateCcw } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import type { ExternalWorkspaceState } from "@/shared/config/external-change";
import { externalAdoptionItems, type ExternalAdoptionItem } from "@/shared/config/external-adoption";
import { getSafeErrorMessage } from "@/shared/api/errors";

type ConfigChange = components["schemas"]["ConfigChange"];

export interface ExternalChangeDrawerProps {
  state: ExternalWorkspaceState;
  onClose: () => void;
  onRestore: () => void;
  onRetryPersistence?: () => void;
  onAdopt?: (changes: ConfigChange[]) => void;
  onDirty?: (dirty: boolean) => void;
}

export function ExternalChangeDrawer({
  state,
  onClose,
  onRestore,
  onRetryPersistence,
  onAdopt,
  onDirty,
}: ExternalChangeDrawerProps) {
  const { modal } = App.useApp();
  const items = useMemo(() => state.diff ? externalAdoptionItems(state.diff) : [], [state.diff]);
  const adoptableKeys = useMemo(() => items.filter((item) => item.change).map((item) => item.key), [items]);
  const revisionKey = state.diff ? `${state.diff.expected.active_revision}:${state.diff.expected.observed_file_revision}` : "none";
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());
  useEffect(() => {
    setSelectedKeys(new Set(adoptableKeys));
  }, [adoptableKeys, revisionKey]);
  const selectedChanges = items.flatMap((item) => item.change && selectedKeys.has(item.key) ? [item.change] : []);
  const closeBlocked = state.phase === "loading" || state.phase === "restoring" || state.phase === "applying";
  const actionBlocked = closeBlocked || state.phase === "awaiting_state";
  const setItemSelected = (key: string, selected: boolean) => {
    setSelectedKeys((current) => {
      const next = new Set(current);
      if (selected) next.add(key); else next.delete(key);
      return next;
    });
    onDirty?.(true);
  };
  const setAllSelected = (selected: boolean) => {
    setSelectedKeys(new Set(selected ? adoptableKeys : []));
    onDirty?.(true);
  };
  const requestClose = () => {
    if (closeBlocked) return;
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
      keyboard={!closeBlocked}
      mask={{ closable: !closeBlocked }}
      onClose={requestClose}
      footer={state.issue ? (
        <Space wrap>
          <Button
            icon={<RotateCcw size={16} aria-hidden="true" />}
            disabled={actionBlocked}
            onClick={confirmRestore}
          >
            还原文件
          </Button>
          {state.issue.kind === "applied_unpersisted" && onRetryPersistence ? (
            <Button
              type="primary"
              loading={state.phase === "restoring"}
              disabled={state.phase === "awaiting_state"}
              onClick={onRetryPersistence}
            >
              重试文件同步
            </Button>
          ) : null}
          {onAdopt ? (
            <Button type="primary" icon={<GitMerge size={16} aria-hidden="true" />} loading={state.phase === "applying"} disabled={actionBlocked || selectedChanges.length === 0 || Boolean(state.diff?.parse_error)} onClick={() => onAdopt(selectedChanges)}>
              组合采用 {selectedChanges.length} 项
            </Button>
          ) : null}
        </Space>
      ) : null}
    >
      <ExternalChangeDrawerBody state={state} items={items} selectedKeys={selectedKeys} onSelect={setItemSelected} onSelectAll={setAllSelected} />
    </Drawer>
  );
}

function ExternalChangeDrawerBody({
  state,
  items,
  selectedKeys,
  onSelect,
  onSelectAll,
}: {
  state: ExternalWorkspaceState;
  items: ExternalAdoptionItem[];
  selectedKeys: Set<string>;
  onSelect: (key: string, selected: boolean) => void;
  onSelectAll: (selected: boolean) => void;
}) {
  if (state.phase === "loading") return <div className="external-change-loading"><Spin /></div>;
  if (state.phase === "error") return <Alert type="error" showIcon message={getSafeErrorMessage(state.error)} />;
  if (state.phase === "conflict") {
    return <Alert type="warning" showIcon message="配置文件已再次变化，请重新读取差异。" />;
  }
  if (!state.diff) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="暂无可显示的差异" />;

  const { diff } = state;
  return (
    <Space direction="vertical" size={20} className="external-change-content">
      {state.phase === "awaiting_state" ? (
        <Alert type="info" showIcon message="文件操作已受理，正在确认权威配置状态。" />
      ) : null}
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
        <Space className="external-change-heading" align="center" wrap>
          <Typography.Title level={5}>可编辑变化</Typography.Title>
          {items.some((item) => item.change) ? (
            <Checkbox
              checked={items.filter((item) => item.change).every((item) => selectedKeys.has(item.key))}
              indeterminate={items.some((item) => item.change && selectedKeys.has(item.key)) && !items.filter((item) => item.change).every((item) => selectedKeys.has(item.key))}
              onChange={(event) => onSelectAll(event.target.checked)}
            >采用全部可用变化</Checkbox>
          ) : null}
        </Space>
        <List
          size="small"
          bordered
          locale={{ emptyText: "无可编辑变化" }}
          dataSource={items}
          renderItem={(item) => (
            <List.Item className="external-change-item">
              <div className="external-change-item-content">
                <Space align="start" wrap>
                  <Checkbox aria-label={`采用 ${item.label}`} checked={item.change !== null && selectedKeys.has(item.key)} disabled={!item.change} onChange={(event) => onSelect(item.key, event.target.checked)} />
                  <Typography.Text strong>{item.label}</Typography.Text>
                  <Tag>{item.action === "create" ? "新增" : item.action === "update" ? "更新" : "不可采用"}</Tag>
                </Space>
                {item.note ? <Typography.Text type="secondary">{item.note}</Typography.Text> : null}
                {item.fields.length > 0 ? (
                  <div className="external-field-diff">
                    {item.fields.map((field) => (
                      <div key={field.path} className="external-field-diff-row">
                        <Typography.Text code>{field.path}</Typography.Text>
                        <Typography.Text title={field.active}>{field.active}</Typography.Text>
                        <Typography.Text type="secondary">→</Typography.Text>
                        <Typography.Text title={field.external}>{field.external}</Typography.Text>
                      </div>
                    ))}
                  </div>
                ) : null}
              </div>
            </List.Item>
          )}
        />
      </div>
    </Space>
  );
}
