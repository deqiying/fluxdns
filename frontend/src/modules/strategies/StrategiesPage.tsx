import { useEffect, useMemo, useState } from "react";
import { Button, Form, Input, Select, Space, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { ArrowDown, ArrowUp, Pencil, Plus, Search, Trash2 } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Strategy = Schemas["Strategy"];

interface RuleFormValue {
  source_type: "hosts" | "rule_set";
  source: string;
  upstream?: string;
}

interface StrategyFormValues {
  name: string;
  default_upstream: string;
  rules: RuleFormValue[];
  cache_mode: "inherit" | "enabled" | "disabled";
  ttl_mode: "inherit" | "enabled" | "disabled";
  ttl_min?: string;
  ttl_max?: string;
  ecs_mode: "inherit" | "disabled" | "client" | "custom";
  ecs_custom_ip?: string;
}

export function StrategiesPage() {
  const query = useConfigModule("strategy");
  const upstreams = useConfigModule("upstreams");
  const hosts = useConfigModule("hosts");
  const mutation = useConfigChangeMutation("strategy");
  const [form] = Form.useForm<StrategyFormValues>();
  const [editing, setEditing] = useState<Strategy | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const ttlMode = Form.useWatch("ttl_mode", form);
  const ecsMode = Form.useWatch("ecs_mode", form);
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "strategy" ? [item.value] : []) ?? [], [query.data]);
  const visible = items.filter((item) => item.name.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  const upstreamOptions = upstreams.data?.values.flatMap((item) => item.module === "upstreams" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const hostsOptions = hosts.data?.values.flatMap((item) => item.module === "hosts" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    if (editing === "create") {
      form.setFieldsValue({ name: "", default_upstream: "", rules: [{ source_type: "hosts", source: "" }], cache_mode: "inherit", ttl_mode: "inherit", ecs_mode: "inherit" });
      return;
    }
    form.setFieldsValue({
      name: editing.name,
      default_upstream: editing.default_upstream,
      rules: editing.rules.map((rule) => rule.hosts
        ? { source_type: "hosts", source: rule.hosts }
        : { source_type: "rule_set", source: rule.rule_set ?? "", upstream: rule.upstream }),
      cache_mode: editing.cache ? (editing.cache.enabled ? "enabled" : "disabled") : "inherit",
      ttl_mode: editing.ttl_override ? (editing.ttl_override.enabled === false ? "disabled" : "enabled") : "inherit",
      ttl_min: editing.ttl_override?.min,
      ttl_max: editing.ttl_override?.max,
      ecs_mode: editing.edns_client_subnet?.mode ?? "inherit",
      ecs_custom_ip: editing.edns_client_subnet?.custom_ip,
    });
  }, [editing, form]);

  const columns: TableColumnsType<Strategy> = [
    { title: "名称", dataIndex: "name", width: 220, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "默认上游", dataIndex: "default_upstream", width: 220, render: (value: string) => <Typography.Text code>{value}</Typography.Text> },
    { title: "有序规则", width: 120, render: (_, item) => `${item.rules.length} 条` },
    { title: "缓存", width: 110, render: (_, item) => <Tag>{item.cache ? (item.cache.enabled ? "启用" : "禁用") : "继承"}</Tag> },
    { title: "TTL", width: 110, render: (_, item) => <Tag>{item.ttl_override ? (item.ttl_override.enabled === false ? "禁用" : "覆盖") : "继承"}</Tag> },
    { title: "引用", width: 80, render: (_, item) => query.data?.references.filter((reference) => reference.to_name === item.name).length ?? 0 },
    { title: "操作", width: 72, align: "center", render: (_, item) => <Tooltip title="编辑策略"><Button type="text" aria-label={`编辑策略 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} /></Tooltip> },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const value: Strategy = {
      name: values.name,
      default_upstream: values.default_upstream,
      rules: values.rules.map((rule) => rule.source_type === "hosts"
        ? { hosts: rule.source }
        : { rule_set: rule.source, upstream: rule.upstream ?? "" }),
      ...(values.cache_mode === "inherit" ? {} : { cache: { enabled: values.cache_mode === "enabled" } }),
      ...(values.ttl_mode === "inherit" ? {} : { ttl_override: values.ttl_mode === "disabled" ? { enabled: false } : { enabled: true, ...(values.ttl_min ? { min: values.ttl_min } : {}), ...(values.ttl_max ? { max: values.ttl_max } : {}) } }),
      ...(values.ecs_mode === "inherit" ? {} : { edns_client_subnet: { mode: values.ecs_mode, ...(values.ecs_mode === "custom" && values.ecs_custom_ip ? { custom_ip: values.ecs_custom_ip } : {}) } }),
    };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "strategy", change: { action: "create", value } }
      : { module: "strategy", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 保留草稿。
    }
  };

  return (
    <PageFrame title="DNS 分流策略" description="按顺序匹配 Hosts 或规则集，并为策略设置默认上游及可选覆盖。" meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined} actions={<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加策略</Button>}>
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? <div className="config-module-content"><div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索策略名称" onChange={(event) => setSearch(event.target.value)} /></div><Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 900 }} locale={{ emptyText: search ? "没有匹配的策略" : "尚未配置策略" }} /></div> : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加策略" : "编辑策略"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}><Input /></Form.Item>
          <Form.Item name="default_upstream" label="默认上游" rules={[{ required: true }]}><Select showSearch options={upstreamOptions} /></Form.Item>
          <Form.List name="rules" rules={[{ validator: async (_, rules) => { if (!rules?.length) throw new Error("至少需要一条规则"); } }]}>
            {(fields, { add, remove, move }, { errors }) => <div className="strategy-rules"><Typography.Text strong>有序规则</Typography.Text>{fields.map((field, index) => <StrategyRuleRow key={field.key} field={field} index={index} count={fields.length} hostsOptions={hostsOptions} upstreamOptions={upstreamOptions} remove={remove} move={move} />)}<Button icon={<Plus size={16} />} onClick={() => add({ source_type: "hosts", source: "" })}>添加规则</Button><Form.ErrorList errors={errors} /></div>}
          </Form.List>
          <Form.Item name="cache_mode" label="缓存覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承全局", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }]} /></Form.Item>
          <Form.Item name="ttl_mode" label="TTL 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承全局", value: "inherit" }, { label: "启用覆盖", value: "enabled" }, { label: "禁用覆盖", value: "disabled" }]} /></Form.Item>
          {ttlMode === "enabled" ? <Space className="paired-fields" align="start"><Form.Item name="ttl_min" label="最小 TTL"><Input placeholder="30s" /></Form.Item><Form.Item name="ttl_max" label="最大 TTL"><Input placeholder="1h" /></Form.Item></Space> : null}
          <Form.Item name="ecs_mode" label="ECS 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承全局", value: "inherit" }, { label: "禁用", value: "disabled" }, { label: "客户端地址", value: "client" }, { label: "自定义", value: "custom" }]} /></Form.Item>
          {ecsMode === "custom" ? <Form.Item name="ecs_custom_ip" label="自定义 ECS" rules={[{ required: true }]}><Input /></Form.Item> : null}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}

function StrategyRuleRow({ field, index, count, hostsOptions, upstreamOptions, remove, move }: { field: { key: number; name: number }; index: number; count: number; hostsOptions: Array<{ label: string; value: string }>; upstreamOptions: Array<{ label: string; value: string }>; remove: (index: number) => void; move: (from: number, to: number) => void }) {
  return (
    <div className="strategy-rule-row">
      <Form.Item name={[field.name, "source_type"]} rules={[{ required: true }]}><Select options={[{ label: "Hosts", value: "hosts" }, { label: "规则集", value: "rule_set" }]} /></Form.Item>
      <Form.Item noStyle shouldUpdate={(previous, current) => previous.rules?.[index]?.source_type !== current.rules?.[index]?.source_type}>
        {({ getFieldValue }) => getFieldValue(["rules", index, "source_type"]) === "hosts"
          ? <Form.Item name={[field.name, "source"]} rules={[{ required: true }]}><Select showSearch options={hostsOptions} placeholder="选择 Hosts" /></Form.Item>
          : <><Form.Item name={[field.name, "source"]} rules={[{ required: true }]}><Input placeholder="规则集或 selector" /></Form.Item><Form.Item name={[field.name, "upstream"]} rules={[{ required: true }]}><Select showSearch options={upstreamOptions} placeholder="匹配上游" /></Form.Item></>}
      </Form.Item>
      <Space size={2}><Tooltip title="上移"><Button type="text" icon={<ArrowUp size={16} />} disabled={index === 0} onClick={() => move(index, index - 1)} /></Tooltip><Tooltip title="下移"><Button type="text" icon={<ArrowDown size={16} />} disabled={index === count - 1} onClick={() => move(index, index + 1)} /></Tooltip><Tooltip title="移除"><Button type="text" danger icon={<Trash2 size={16} />} onClick={() => remove(index)} /></Tooltip></Space>
    </div>
  );
}
