import { useEffect, useMemo, useState } from "react";
import { Alert, App, Button, Form, Input, Select, Space, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import type { Rule } from "antd/es/form";
import { Pencil, Plus, Search, Undo2 } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigSyncBadge } from "@/shared/components/ConfigStateSummary";
import { DurationInput, durationOptionalRules } from "@/shared/components/DurationInput";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { clientEditValue } from "@/shared/config/contract";
import { normalizeDuration } from "@/shared/config/form-values";
import { configStateEditable, useConfigChangeMutation, useConfigModule, useConfigState } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Client = Schemas["Client"];

interface ClientFormValues {
  name: string;
  client_id: string;
  ips: string[];
  strategy?: string;
  cache_mode: "inherit" | "enabled" | "disabled";
  ttl_mode: "inherit" | "enabled" | "disabled";
  ttl_min?: string;
  ttl_max?: string;
  ecs_mode: "inherit" | "disabled" | "client" | "custom";
  ecs_custom_ip?: string;
}

/** 与后端 valid_client_id 一致：1-128 个 URL unreserved ASCII，大小写敏感。 */
const CLIENT_ID_PATTERN = /^[A-Za-z0-9._~-]{1,128}$/;

export function ClientsPage() {
  const query = useConfigModule("clients");
  const strategies = useConfigModule("strategy");
  const mutation = useConfigChangeMutation("clients");
  const state = useConfigState();
  const { modal } = App.useApp();
  const [form] = Form.useForm<ClientFormValues>();
  const ttlMode = Form.useWatch("ttl_mode", form);
  const ecsMode = Form.useWatch("ecs_mode", form);
  const clientId = Form.useWatch("client_id", form);
  const [editing, setEditing] = useState<Client | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  // 编辑已有客户端时 ID 默认锁定，显式解锁后才允许修改请求身份。
  const [idUnlocked, setIdUnlocked] = useState(false);
  const [search, setSearch] = useState("");
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "clients" ? [item.value] : []) ?? [], [query.data]);
  const visible = items.filter((item) => `${item.name} ${item.client_id} ${item.match?.ips?.join(" ") ?? ""}`.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  const strategyOptions = strategies.data?.values.flatMap((item) => item.module === "strategy" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const originalId = editing && editing !== "create" ? editing.client_id : null;
  const idChanged = originalId !== null && typeof clientId === "string" && clientId !== originalId;
  // 唯一性只做本地预检，最终以后端整份候选校验为准。
  const clientIdRules: Rule[] = [
    { required: true, message: "客户端 ID 不能为空" },
    { pattern: CLIENT_ID_PATTERN, message: "仅支持 A-Z a-z 0-9 . _ ~ -，长度 1-128，区分大小写" },
    {
      validator: async (_, value?: string) => {
        if (!value || value === originalId) return;
        const owner = items.find((item) => item.client_id === value);
        if (owner) throw new Error(`已被客户端 “${owner.name}” 使用`);
      },
    },
  ];

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    setIdUnlocked(false);
    if (editing === "create") {
      form.setFieldsValue({ name: "", client_id: "", ips: [], cache_mode: "inherit", ttl_mode: "inherit", ecs_mode: "inherit" });
    } else {
      form.setFieldsValue({
        name: editing.name,
        client_id: editing.client_id,
        ips: editing.match?.ips ?? [],
        strategy: editing.strategy,
        cache_mode: editing.cache ? (editing.cache.enabled ? "enabled" : "disabled") : "inherit",
        ttl_mode: editing.ttl_override ? (editing.ttl_override.enabled === false ? "disabled" : "enabled") : "inherit",
        ttl_min: normalizeDuration(editing.ttl_override?.min),
        ttl_max: normalizeDuration(editing.ttl_override?.max),
        ecs_mode: editing.edns_client_subnet?.mode ?? "inherit",
        ecs_custom_ip: editing.edns_client_subnet?.custom_ip,
      });
    }
  }, [editing, form]);

  const columns: TableColumnsType<Client> = [
    { title: "客户端", width: 260, render: (_, item) => <div className="query-cell"><Typography.Text strong>{item.name}</Typography.Text><Typography.Text type="secondary" code>{item.client_id}</Typography.Text></div> },
    { title: "IP / CIDR", render: (_, item) => item.match?.ips?.length ? <Space wrap>{item.match.ips.map((ip) => <Tag key={ip}>{ip}</Tag>)}</Space> : <Typography.Text type="secondary">未配置</Typography.Text> },
    { title: "策略", dataIndex: "strategy", width: 180, render: (value?: string) => value ?? "继承默认" },
    { title: "缓存", width: 100, render: (_, item) => item.cache ? (item.cache.enabled ? "启用" : "禁用") : "继承" },
    { title: "操作", width: 72, align: "center", render: (_, item) => <Tooltip title="编辑客户端"><Button type="text" aria-label={`编辑客户端 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} /></Tooltip> },
  ];

  /** 修改 ID 会让旧身份的请求不再命中本客户端，保存前需要单独确认；历史记录不迁移。 */
  const confirmClientIdChange = (previous: string, next: string) => new Promise<boolean>((resolve) => {
    modal.confirm({
      title: "确认修改客户端 ID？",
      okText: "确认修改",
      cancelText: "取消",
      content: (
        <Space orientation="vertical" size={8}>
          <Typography.Text>原 ID：<Typography.Text code delete>{previous}</Typography.Text></Typography.Text>
          <Typography.Text>新 ID：<Typography.Text code>{next}</Typography.Text></Typography.Text>
          <Typography.Text type="secondary">终端需改用新的 DoH 路径；使用旧 ID 的请求将不再匹配本客户端。历史查询与统计仍归属旧 ID，不会迁移。</Typography.Text>
        </Space>
      ),
      onOk: () => resolve(true),
      onCancel: () => resolve(false),
    });
  });

  const resetClientId = () => {
    if (originalId === null) return;
    form.setFieldValue("client_id", originalId);
    void form.validateFields(["client_id"]);
    setIdUnlocked(false);
  };

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const common = {
      name: values.name,
      match: { ips: values.ips ?? [] },
      ...(values.strategy ? { strategy: values.strategy } : {}),
      ...(values.cache_mode === "inherit" ? {} : { cache: { enabled: values.cache_mode === "enabled" } }),
      ...(values.ttl_mode === "inherit" ? {} : { ttl_override: values.ttl_mode === "disabled" ? { enabled: false } : { enabled: true, ...(values.ttl_min ? { min: values.ttl_min } : {}), ...(values.ttl_max ? { max: values.ttl_max } : {}) } }),
      ...(values.ecs_mode === "inherit" ? {} : { edns_client_subnet: { mode: values.ecs_mode, ...(values.ecs_mode === "custom" && values.ecs_custom_ip ? { custom_ip: values.ecs_custom_ip } : {}) } }),
    };
    let change: Schemas["ConfigChange"];
    if (editing === "create") {
      change = { module: "clients", change: { action: "create", value: { ...common, client_id: values.client_id } } };
    } else {
      // 只有 ID 实际变化时才提交 client_id；未变化的编辑保持原有白名单形状。
      const nextId = values.client_id !== editing.client_id ? values.client_id : undefined;
      if (nextId && !(await confirmClientIdChange(editing.client_id, nextId))) return;
      const value: Schemas["ClientEdit"] = { ...clientEditValue({ ...common, client_id: editing.client_id }), ...(nextId ? { client_id: nextId } : {}) };
      change = { module: "clients", change: { action: "update", original_name: editing.name, value } };
    }
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 冲突与校验失败时保留草稿。
    }
  };

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
      {query.data ? <div className="config-module-content"><div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索 name、ID 或 IP" onChange={(event) => setSearch(event.target.value)} /></div><Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 920 }} locale={{ emptyText: search ? "没有匹配的客户端" : "尚未配置客户端" }} /></div> : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加客户端" : "编辑客户端"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="管理名称" rules={[{ required: true }, { max: 128 }]}><Input /></Form.Item>
          {/* 外层只负责标签、必填标记和错误展示；noStyle 子项承载值与校验，以便与操作按钮组成 Space.Compact。 */}
          <Form.Item label="客户端 ID" htmlFor="client_id" required extra="终端的请求身份：DoH 路径中的 {client_id} 段或精确 ID 匹配">
            <Space.Compact block>
              <Form.Item name="client_id" noStyle rules={clientIdRules}>
                <Input disabled={editing !== "create" && !idUnlocked} placeholder="例如 phone-xiao" />
              </Form.Item>
              {editing === "create" ? null : idUnlocked
                ? <Button icon={<Undo2 size={14} />} aria-label="撤销客户端 ID 修改" onClick={resetClientId}>撤销</Button>
                : <Button icon={<Pencil size={14} />} aria-label="修改客户端 ID" onClick={() => setIdUnlocked(true)}>修改</Button>}
            </Space.Compact>
          </Form.Item>
          {idChanged ? (
            <Alert
              type="warning"
              showIcon
              style={{ marginBottom: 24 }}
              title="修改客户端 ID 会改变该终端的请求身份"
              description="终端需改用新的 DoH 路径，使用旧 ID 的请求将不再匹配本客户端；历史查询与统计仍归属旧 ID，不会迁移；该客户端的缓存将按新身份重新建立。"
            />
          ) : null}
          <Form.Item name="ips" label="IP / CIDR"><Select mode="tags" tokenSeparators={[","]} /></Form.Item>
          <Form.Item name="strategy" label="策略"><Select allowClear showSearch options={strategyOptions} placeholder="继承默认" /></Form.Item>
          <Form.Item name="cache_mode" label="缓存覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }]} /></Form.Item>
          <Form.Item name="ttl_mode" label="TTL 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用覆盖", value: "enabled" }, { label: "禁用覆盖", value: "disabled" }]} /></Form.Item>
          {ttlMode === "enabled" ? <Space className="paired-fields" align="start"><Form.Item name="ttl_min" label="最小 TTL" rules={durationOptionalRules}><DurationInput label="最小 TTL" /></Form.Item><Form.Item name="ttl_max" label="最大 TTL" rules={durationOptionalRules}><DurationInput label="最大 TTL" /></Form.Item></Space> : null}
          <Form.Item name="ecs_mode" label="ECS 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "禁用", value: "disabled" }, { label: "客户端地址", value: "client" }, { label: "自定义", value: "custom" }]} /></Form.Item>
          {ecsMode === "custom" ? <Form.Item name="ecs_custom_ip" label="自定义 ECS" rules={[{ required: true }]}><Input /></Form.Item> : null}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
