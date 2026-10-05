import { useEffect, useMemo, useRef, useState } from "react";
import { Button, Input, Segmented, Space, Table, Tooltip, Typography, type TableColumnsType } from "antd";
import { Pencil, Plus, Search } from "lucide-react";
import { ConfigSyncBadge } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigModule, useConfigState } from "@/shared/config/hooks";
import { ClientFormDrawer, type ClientEditorTarget } from "./ClientFormDrawer";
import { CopyButton } from "./CopyButton";
import { clientAvatar, clientOverrides, clientRouteTemplates, clientSummary, type Client } from "./client-view";
import "./clients.css";

type ClientFilter = "all" | "strategy" | "overrides";

/** IP/CIDR 列最多直接展示的条目数，其余折叠为 +N 并在悬停时展开。 */
const VISIBLE_IPS = 2;
/** 保存成功后对应行的高亮时长。 */
const HIGHLIGHT_MS = 2000;

export function ClientsPage() {
  const query = useConfigModule("clients");
  const strategies = useConfigModule("strategy");
  // 监听入口只用于按 DoH 路由模板生成新 ID 的地址提示，读取失败不阻塞本页。
  const listeners = useConfigModule("listener");
  const state = useConfigState();
  const [editing, setEditing] = useState<ClientEditorTarget | null>(null);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<ClientFilter>("all");
  const [highlighted, setHighlighted] = useState<string | null>(null);
  const highlightTimer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(highlightTimer.current), []);

  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "clients" ? [item.value] : []) ?? [], [query.data]);
  const summary = useMemo(() => clientSummary(items), [items]);
  const routeTemplates = useMemo(
    () => clientRouteTemplates(listeners.data?.values.flatMap((item) => item.module === "listener" ? [item.value] : []) ?? []),
    [listeners.data],
  );
  const strategyOptions = strategies.data?.values.flatMap((item) => item.module === "strategy" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const keyword = search.trim().toLocaleLowerCase();
  const visible = items.filter((item) => {
    if (filter === "strategy" && !item.strategy) return false;
    if (filter === "overrides" && clientOverrides(item).length === 0) return false;
    return !keyword || `${item.name} ${item.client_id} ${item.match?.ips?.join(" ") ?? ""}`.toLocaleLowerCase().includes(keyword);
  });
  const editingName = editing && editing !== "create" ? editing.name : null;

  const handleSaved = (name: string) => {
    setEditing(null);
    setHighlighted(name);
    window.clearTimeout(highlightTimer.current);
    highlightTimer.current = window.setTimeout(() => setHighlighted(null), HIGHLIGHT_MS);
  };

  const columns: TableColumnsType<Client> = [
    { title: "客户端", key: "client", width: 300, render: (_, item) => <ClientIdentity client={item} /> },
    { title: "IP / CIDR", key: "ips", render: (_, item) => <IpList ips={item.match?.ips ?? []} /> },
    {
      title: "策略",
      key: "strategy",
      width: 150,
      render: (_, item) => item.strategy
        ? <span className="client-strategy-tag">{item.strategy}</span>
        : <span className="client-strategy-tag client-strategy-inherit">继承默认</span>,
    },
    { title: "覆盖项", key: "overrides", width: 260, render: (_, item) => <OverrideList client={item} /> },
    {
      title: "操作",
      key: "actions",
      width: 72,
      align: "center",
      render: (_, item) => (
        <Tooltip title="编辑客户端">
          <Button type="text" aria-label={`编辑客户端 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} />
        </Tooltip>
      ),
    },
  ];

  return (
    <PageFrame
      title="客户端配置"
      description="每一台终端，各得其所。"
      actions={(
        <Space size={12}>
          {state.data ? <ConfigSyncBadge state={state.data} /> : null}
          <Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加客户端</Button>
        </Space>
      )}
    >
      <PageState loading={query.isLoading} error={query.error} hasData={!!query.data} onRetry={() => void query.refetch()} />
      {query.data ? (
        <>
          <div className="client-summary">
            <SummaryCard label="客户端" value={summary.total} hint={`其中 ${summary.idOnly} 个仅按 ID 匹配`} />
            <SummaryCard label="自定义策略" value={summary.customStrategy} hint={`另 ${summary.total - summary.customStrategy} 个继承默认策略`} />
            <SummaryCard label="含覆盖项" value={summary.withOverrides} hint="缓存 / TTL / ECS" />
            <SummaryCard label="IP / CIDR 条目" value={summary.ipEntries} hint={`分布于 ${summary.clientsWithIps} 个客户端`} />
          </div>
          <div className="config-module-content">
            <div className="config-table-toolbar client-toolbar">
              <Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索名称、客户端 ID 或 IP" onChange={(event) => setSearch(event.target.value)} />
              <Segmented<ClientFilter>
                value={filter}
                onChange={setFilter}
                options={[
                  { label: `全部 ${summary.total}`, value: "all" },
                  { label: `自定义策略 ${summary.customStrategy}`, value: "strategy" },
                  { label: `含覆盖 ${summary.withOverrides}`, value: "overrides" },
                ]}
              />
              <Typography.Text type="secondary" className="client-toolbar-count">共 {visible.length} 个客户端</Typography.Text>
            </div>
            <Table
              className="client-table"
              rowKey="name"
              columns={columns}
              dataSource={visible}
              rowClassName={(item) => item.name === highlighted ? "client-row-highlight" : item.name === editingName ? "client-row-editing" : ""}
              pagination={{ pageSize: 20, hideOnSinglePage: true }}
              scroll={{ x: 980 }}
              locale={{ emptyText: keyword || filter !== "all" ? "没有匹配的客户端" : "尚未配置客户端" }}
            />
          </div>
        </>
      ) : null}
      <ClientFormDrawer
        target={editing}
        items={items}
        strategyOptions={strategyOptions}
        routeTemplates={routeTemplates}
        configState={query.data?.state}
        onClose={() => setEditing(null)}
        onSaved={handleSaved}
      />
    </PageFrame>
  );
}

function SummaryCard({ label, value, hint }: { label: string; value: number; hint: string }) {
  return (
    <div className="client-summary-card" role="group" aria-label={label}>
      <span className="client-summary-label">{label}</span>
      <span className="client-summary-body">
        <strong>{value}</strong>
        <span>{hint}</span>
      </span>
    </div>
  );
}

/** 名称 + 单行等宽 ID，过长 ID 省略显示，完整值通过 title 或复制按钮获取。 */
function ClientIdentity({ client }: { client: Client }) {
  const avatar = clientAvatar(client.name);
  return (
    <div className="client-identity">
      <span className={`client-avatar client-avatar-${avatar.tone}`} aria-hidden="true">{avatar.text}</span>
      <div className="client-identity-text">
        <Typography.Text strong className="client-name">{client.name}</Typography.Text>
        <span className="client-id-line">
          <span className="client-id-badge">ID</span>
          <code className="client-id-value" title={client.client_id}>{client.client_id}</code>
          <CopyButton text={client.client_id} label={`复制客户端 ID ${client.client_id}`} />
        </span>
      </div>
    </div>
  );
}

function IpList({ ips }: { ips: string[] }) {
  if (ips.length === 0) return <Typography.Text type="secondary">仅按 ID 匹配</Typography.Text>;
  const rest = ips.slice(VISIBLE_IPS);
  return (
    <span className="client-chip-list">
      {ips.slice(0, VISIBLE_IPS).map((ip) => <code key={ip} className="client-ip-chip">{ip}</code>)}
      {rest.length > 0 ? (
        <Tooltip title={<div className="client-ip-tooltip">{rest.map((ip) => <div key={ip}>{ip}</div>)}</div>}>
          <span className="client-ip-more" tabIndex={0} aria-label={`另有 ${rest.length} 个：${rest.join("、")}`}>+{rest.length}</span>
        </Tooltip>
      ) : null}
    </span>
  );
}

/** 只列出未继承的缓存 / TTL / ECS，避免把“继承”铺满整列。 */
function OverrideList({ client }: { client: Client }) {
  const overrides = clientOverrides(client);
  if (overrides.length === 0) return <Typography.Text type="secondary">全部继承</Typography.Text>;
  return (
    <span className="client-chip-list">
      {overrides.map((override) => (
        <span key={override.key} className="client-override-chip"><b>{override.label}</b> {override.value}</span>
      ))}
    </span>
  );
}
