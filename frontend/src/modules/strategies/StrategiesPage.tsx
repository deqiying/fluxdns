import { useEffect, useMemo, useRef, useState } from "react";
import { Button, Form, Input, Select, Space, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { ArrowDown, ArrowUp, ChevronDown, ChevronRight, Pencil, Plus, Search, Trash2 } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ApiError } from "@/shared/api/errors";
import { ConfigSyncBadge } from "@/shared/components/ConfigStateSummary";
import { DurationInput, durationOptionalRules, durationRequiredRules } from "@/shared/components/DurationInput";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule, useConfigState } from "@/shared/config/hooks";
import { ecsSummary, strategyFieldErrors, strategyFromForm, strategyToForm, type RuleFormValue, type Strategy, type StrategyFormValues } from "./strategy-form";
import "./strategies.css";

type Schemas = components["schemas"];

const ecsOptions = [
  { label: "继承", value: "inherit" },
  { label: "禁用", value: "disabled" },
  { label: "客户端地址", value: "client" },
  { label: "自定义", value: "custom" },
];

export function StrategiesPage() {
  const query = useConfigModule("strategy");
  const upstreams = useConfigModule("upstreams");
  const hosts = useConfigModule("hosts");
  const mutation = useConfigChangeMutation("strategy");
  const state = useConfigState();
  const [form] = Form.useForm<StrategyFormValues>();
  const [editing, setEditing] = useState<Strategy | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const openedState = useRef<Schemas["ConfigState"] | null>(null);
  const [revealErrors, setRevealErrors] = useState(0);
  const ttlMode = Form.useWatch("ttl_mode", form);
  const cacheMode = Form.useWatch("cache_mode", form);
  const optimisticMode = Form.useWatch("cache_optimistic_mode", form);
  const ecsMode = Form.useWatch("ecs_mode", form);
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "strategy" ? [item.value] : []) ?? [], [query.data]);
  const visible = items.filter((item) => item.name.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  const upstreamOptions = upstreams.data?.values.flatMap((item) => item.module === "upstreams" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];
  const hostsOptions = hosts.data?.values.flatMap((item) => item.module === "hosts" ? [{ label: item.value.name, value: item.value.name }] : []) ?? [];

  useEffect(() => {
    if (!editing) return;
    openedState.current = query.data?.state ?? null;
    setRevealErrors(0);
    mutation.reset();
    setDirty(false);
    form.resetFields();
    form.setFieldsValue(strategyToForm(editing === "create" ? undefined : editing));
  }, [editing, form]);

  useEffect(() => {
    const errors = mutation.error instanceof ApiError ? strategyFieldErrors(mutation.error.fieldErrors) : [];
    if (errors.length) { form.setFields(errors); setRevealErrors((value) => value + 1); }
  }, [form, mutation.error]);

  const columns: TableColumnsType<Strategy> = [
    { title: "名称", dataIndex: "name", width: 200, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "默认上游", dataIndex: "default_upstream", width: 180, render: (value: string) => <Typography.Text code>{value}</Typography.Text> },
    { title: "有序规则", width: 110, render: (_, item) => `${item.rules.length} 条` },
    { title: "ECS", width: 150, render: (_, item) => <Typography.Text type="secondary">{ecsSummary(item.edns_client_subnet)}</Typography.Text> },
    { title: "缓存", width: 100, render: (_, item) => <Tag>{item.cache ? (item.cache.enabled ? "启用" : "禁用") : "继承"}</Tag> },
    { title: "TTL", width: 100, render: (_, item) => <Tag>{item.ttl_override ? (item.ttl_override.enabled === false ? "禁用" : "覆盖") : "继承"}</Tag> },
    { title: "引用", width: 70, render: (_, item) => query.data?.references.filter((reference) => reference.to_name === item.name).length ?? 0 },
    { title: "操作", width: 72, align: "center", render: (_, item) => <Tooltip title="编辑策略"><Button type="text" aria-label={`编辑策略 ${item.name}`} icon={<Pencil size={17} />} disabled={!!query.error || !!state.error || !query.data || !configStateEditable(query.data.state)} onClick={() => setEditing(item)} /></Tooltip> },
  ];

  const submit = async () => {
    if (!openedState.current || !editing || query.error || state.error) return;
    let values: StrategyFormValues;
    try { await form.validateFields(); values = form.getFieldsValue(true); }
    catch { setRevealErrors((value) => value + 1); return; }
    const value = strategyFromForm(values, editing === "create" ? undefined : editing);
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "strategy", change: { action: "create", value } }
      : { module: "strategy", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: openedState.current });
      if (operation) setEditing(null);
    } catch {
      // 保留草稿与定位后的字段错误。
    }
  };

  return (
    <PageFrame
      title="DNS 分流策略"
      description="每一条规则，各归其位。"
      actions={<Space size={12}>{state.data ? <ConfigSyncBadge state={state.data} /> : null}<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !!query.error || !!state.error || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加策略</Button></Space>}
    >
      <PageState loading={query.isLoading} error={query.error} hasData={!!query.data} onRetry={() => void query.refetch()} />
      {query.data ? <div className="config-module-content strategy-page"><div className="config-table-toolbar"><Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索策略名称" onChange={(event) => setSearch(event.target.value)} /></div><Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 1040 }} locale={{ emptyText: search ? "没有匹配的策略" : "尚未配置策略" }} /></div> : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加策略" : "编辑策略"} dirty={dirty} busy={mutation.isPending} submitDisabled={!query.data || !!query.error || !!state.error} error={mutation.error} layout="wide" onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <section className="strategy-form-section"><Typography.Title level={5}>基础配置</Typography.Title><Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}><Input /></Form.Item><Form.Item name="default_upstream" label="默认上游" rules={[{ required: true }]}><Select showSearch options={upstreamOptions} /></Form.Item></section>
          <section className="strategy-form-section"><Form.List name="rules" rules={[{ validator: async (_, rules) => { if (!rules?.length) throw new Error("至少需要一条规则"); } }]}>{(fields, { add, remove, move }, { errors }) => <div className="strategy-rules"><div className="strategy-section-heading"><Typography.Title level={5}>有序规则</Typography.Title><Button icon={<Plus size={16} />} onClick={() => add({ source_type: "hosts", source: "", ecs_mode: "inherit" })}>添加规则</Button></div>{fields.map((field, index) => <StrategyRuleCard key={field.key} field={field} index={index} count={fields.length} form={form} revealErrors={revealErrors} hostsOptions={hostsOptions} upstreamOptions={upstreamOptions} remove={remove} move={move} />)}<Form.ErrorList errors={errors} /></div>}</Form.List></section>
          <section className="strategy-form-section"><Typography.Title level={5}>策略级覆盖</Typography.Title><Form.Item name="cache_mode" label="缓存覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }]} /></Form.Item>{cacheMode !== "inherit" ? <><Form.Item name="cache_optimistic_mode" label="乐观缓存" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用", value: "enabled" }, { label: "禁用", value: "disabled" }]} /></Form.Item>{optimisticMode !== "inherit" ? <Space className="paired-fields" align="start"><Form.Item name="cache_answer_ttl" label="回答 TTL" rules={durationRequiredRules}><DurationInput label="回答 TTL" /></Form.Item><Form.Item name="cache_max_age" label="最大陈旧时间" rules={durationRequiredRules}><DurationInput label="最大陈旧时间" /></Form.Item><Form.Item name="cache_negative_max_age" label="空应答最大陈旧时间" tooltip="NODATA/NXDOMAIN 过期后仍可乐观返回的最长时间；0 表示过期即回源，留空使用默认 5m，超过最大陈旧时间时按其截断" rules={durationOptionalRules}><DurationInput label="空应答最大陈旧时间" /></Form.Item></Space> : null}</> : null}<Form.Item name="ttl_mode" label="TTL 覆盖" rules={[{ required: true }]}><Select options={[{ label: "继承", value: "inherit" }, { label: "启用覆盖", value: "enabled" }, { label: "禁用覆盖", value: "disabled" }]} /></Form.Item>{ttlMode === "enabled" ? <Space className="paired-fields" align="start"><Form.Item name="ttl_min" label="最小 TTL" rules={durationOptionalRules}><DurationInput label="最小 TTL" /></Form.Item><Form.Item name="ttl_max" label="最大 TTL" rules={durationOptionalRules}><DurationInput label="最大 TTL" /></Form.Item></Space> : null}<Form.Item name="ecs_mode" label="ECS 覆盖" rules={[{ required: true }]}><Select options={ecsOptions} /></Form.Item>{ecsMode === "custom" ? <Form.Item name="ecs_custom_ip" label="自定义 ECS" rules={[{ required: true, message: "请输入 IP 或 CIDR" }]}><Input /></Form.Item> : null}</section>
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}

function StrategyRuleCard({ field, index, count, form, revealErrors, hostsOptions, upstreamOptions, remove, move }: { field: { key: number; name: number }; index: number; count: number; form: ReturnType<typeof Form.useForm<StrategyFormValues>>[0]; revealErrors: number; hostsOptions: Array<{ label: string; value: string }>; upstreamOptions: Array<{ label: string; value: string }>; remove: (index: number) => void; move: (from: number, to: number) => void }) {
  const [expanded, setExpanded] = useState(false);
  useEffect(() => { if (revealErrors) setExpanded(true); }, [revealErrors]);
  const rules = Form.useWatch("rules", form);
  const rule: RuleFormValue | undefined = rules?.[field.name] ?? form.getFieldValue(["rules", field.name]);
  const changeSourceType = (sourceType: RuleFormValue["source_type"]) => form.setFieldValue(["rules", field.name], { source_type: sourceType, source: "", ecs_mode: rule?.ecs_mode ?? "inherit", ecs_custom_ip: rule?.ecs_custom_ip });
  const sourceLabel = rule?.source_type === "hosts" ? "Hosts" : "规则集";
  const target = rule?.source_type === "hosts" ? rule?.source : `${rule?.source || "未选择规则集"} → ${rule?.upstream || "未选择上游"}`;
  const ruleEcs = !rule || rule.ecs_mode === "inherit" ? undefined : { mode: rule.ecs_mode, custom_ip: rule.ecs_custom_ip };
  return (
    <article className="strategy-rule-card">
      <div className="strategy-rule-summary">
        <Button type="text" className="strategy-rule-toggle" aria-expanded={expanded} onClick={() => setExpanded((value) => !value)} icon={expanded ? <ChevronDown size={16} /> : <ChevronRight size={16} />}>{index + 1}. {sourceLabel}</Button>
        <Typography.Text ellipsis className="strategy-rule-target">{target}</Typography.Text>
        <Typography.Text type="secondary" className="strategy-rule-ecs">{ecsSummary(ruleEcs)}</Typography.Text>
        <RuleActions index={index} count={count} remove={remove} move={move} />
      </div>
      <div className="strategy-rule-details" style={expanded ? undefined : { display: "none" }}>
        <Form.Item name={[field.name, "source_type"]} label="类型" rules={[{ required: true }]}><Select options={[{ label: "Hosts", value: "hosts" }, { label: "规则集", value: "rule_set" }]} onChange={changeSourceType} /></Form.Item>
        {rule?.source_type === "hosts" ? <>
          <Form.Item name={[field.name, "source"]} label="Hosts" rules={[{ required: true }]}><Select showSearch options={hostsOptions} placeholder="选择 Hosts" /></Form.Item>
          <Typography.Text type="secondary">Hosts 本地回答不向上游发送 ECS。</Typography.Text>
        </> : <>
          <Form.Item name={[field.name, "source"]} label="规则集或 selector" rules={[{ required: true }]}><Input /></Form.Item>
          <Form.Item name={[field.name, "upstream"]} label="匹配上游" rules={[{ required: true }]}><Select showSearch options={upstreamOptions} placeholder="选择上游" /></Form.Item>
        </>}
        <Form.Item name={[field.name, "ecs_mode"]} label="ECS 覆盖" rules={[{ required: true }]}><Select options={ecsOptions} /></Form.Item>
        {rule?.ecs_mode === "custom" ? <Form.Item name={[field.name, "ecs_custom_ip"]} label="自定义 ECS" rules={[{ required: true, message: "请输入 IP 或 CIDR" }]}><Input /></Form.Item> : null}
      </div>
    </article>
  );
}

function RuleActions({ index, count, remove, move }: { index: number; count: number; remove: (index: number) => void; move: (from: number, to: number) => void }) {
  return <Space size={2}><Tooltip title="上移"><Button type="text" aria-label={`上移规则 ${index + 1}`} icon={<ArrowUp size={16} />} disabled={index === 0} onClick={() => move(index, index - 1)} /></Tooltip><Tooltip title="下移"><Button type="text" aria-label={`下移规则 ${index + 1}`} icon={<ArrowDown size={16} />} disabled={index === count - 1} onClick={() => move(index, index + 1)} /></Tooltip><Tooltip title="移除"><Button type="text" aria-label={`移除规则 ${index + 1}`} danger icon={<Trash2 size={16} />} onClick={() => remove(index)} /></Tooltip></Space>;
}
