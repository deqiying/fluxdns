import { useEffect, useMemo, useState } from "react";
import { Button, Form, Input, Segmented, Select, Switch, Table, Tag, Tooltip, Typography, type TableColumnsType } from "antd";
import { Pencil, Plus, Search } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Hosts = Schemas["Hosts"];

interface HostsFormValues {
  name: string;
  type: "const" | "file";
  format: "json" | "hosts";
  content?: string;
  path?: string;
  auto_update?: boolean;
  update_interval?: string;
}

export function HostsPage() {
  const query = useConfigModule("hosts");
  const mutation = useConfigChangeMutation("hosts");
  const [form] = Form.useForm<HostsFormValues>();
  const sourceType = Form.useWatch("type", form);
  const [editing, setEditing] = useState<Hosts | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const items = useMemo(() => query.data?.values.flatMap((item) => item.module === "hosts" ? [item.value] : []) ?? [], [query.data]);
  const visible = items.filter((item) => item.name.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));

  useEffect(() => {
    if (!editing) return;
    mutation.reset();
    setDirty(false);
    if (editing === "create") {
      form.setFieldsValue({ name: "", type: "const", format: "hosts", content: "", auto_update: false });
    } else if (editing.type === "const") {
      form.setFieldsValue({ name: editing.name, type: "const", format: editing.format, content: editing.hosts });
    } else {
      form.setFieldsValue({
        name: editing.name,
        type: "file",
        format: editing.format,
        path: editing.path,
        auto_update: editing.auto_update,
        update_interval: editing.update_interval,
      });
    }
  }, [editing, form]);

  const columns: TableColumnsType<Hosts> = [
    { title: "名称", dataIndex: "name", width: 220, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "来源", width: 130, render: (_, item) => <Tag color={item.type === "const" ? "blue" : undefined}>{item.type === "const" ? "内联" : "文件"}</Tag> },
    { title: "格式", dataIndex: "format", width: 100, render: (value: string) => <Tag>{value.toUpperCase()}</Tag> },
    {
      title: "内容或路径",
      ellipsis: true,
      render: (_, item) => item.type === "const"
        ? <Typography.Text type="secondary">{item.hosts.split(/\r?\n/).filter(Boolean).length} 行内联映射</Typography.Text>
        : <Typography.Text code>{item.path}</Typography.Text>,
    },
    {
      title: "状态",
      width: 130,
      render: (_, item) => {
        const runtime = query.data?.runtime.find((value) => value.module === "hosts" && value.name === item.name);
        return runtime && runtime.module === "hosts" ? <Tag color={runtime.condition === "ready" ? "success" : "warning"}>{runtime.condition}</Tag> : <Tag>未知</Tag>;
      },
    },
    { title: "引用", width: 80, render: (_, item) => query.data?.references.filter((reference) => reference.to_name === item.name).length ?? 0 },
    {
      title: "操作", width: 72, align: "center", render: (_, item) => (
        <Tooltip title="编辑 Hosts">
          <Button type="text" aria-label={`编辑 Hosts ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} />
        </Tooltip>
      ),
    },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const value: Hosts = values.type === "const"
      ? { name: values.name, type: "const", format: values.format, hosts: values.content ?? "" }
      : {
          name: values.name,
          type: "file",
          format: values.format,
          path: values.path ?? "",
          auto_update: values.auto_update ?? false,
          ...(values.auto_update && values.update_interval ? { update_interval: values.update_interval } : {}),
        };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "hosts", change: { action: "create", value } }
      : { module: "hosts", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // 失败由表单容器呈现并保留草稿。
    }
  };

  return (
    <PageFrame
      title="Hosts 配置"
      description="管理内联或本地文件 Hosts 资源；运行状态来自当前 Runtime 快照。"
      meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined}
      actions={<Button type="primary" icon={<Plus size={17} />} disabled={!query.data || !configStateEditable(query.data.state)} onClick={() => setEditing("create")}>添加 Hosts</Button>}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? (
        <div className="config-module-content">
          <div className="config-table-toolbar">
            <Input allowClear value={search} prefix={<Search size={16} />} placeholder="搜索 Hosts 名称" onChange={(event) => setSearch(event.target.value)} />
          </div>
          <Table rowKey="name" columns={columns} dataSource={visible} pagination={{ pageSize: 20, hideOnSinglePage: true }} scroll={{ x: 900 }} locale={{ emptyText: search ? "没有匹配的 Hosts" : "尚未配置 Hosts" }} />
        </div>
      ) : null}
      <ConfigFormModal open={editing !== null} title={editing === "create" ? "添加 Hosts" : "编辑 Hosts"} dirty={dirty} busy={mutation.isPending} error={mutation.error} onCancel={() => setEditing(null)} onSubmit={() => void submit()}>
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}><Input autoComplete="off" /></Form.Item>
          <Form.Item name="type" label="来源" rules={[{ required: true }]}><Segmented block options={[{ label: "内联", value: "const" }, { label: "本地文件", value: "file" }]} /></Form.Item>
          <Form.Item name="format" label="格式" rules={[{ required: true }]}><Select options={[{ label: "Hosts 行格式", value: "hosts" }, { label: "JSON", value: "json" }]} /></Form.Item>
          {sourceType === "file" ? (
            <>
              <Form.Item name="path" label="文件路径" rules={[{ required: true }, { max: 4096 }]}><Input autoComplete="off" /></Form.Item>
              <Form.Item name="auto_update" label="自动重新加载" valuePropName="checked"><Switch /></Form.Item>
              <Form.Item noStyle shouldUpdate={(previous, current) => previous.auto_update !== current.auto_update}>
                {({ getFieldValue }) => getFieldValue("auto_update") ? (
                  <Form.Item name="update_interval" label="检查周期" rules={[{ required: true }]}><Input placeholder="5m" /></Form.Item>
                ) : null}
              </Form.Item>
            </>
          ) : (
            <Form.Item name="content" label="内联映射" rules={[{ required: true }, { max: 262144 }]}>
              <Input.TextArea autoSize={{ minRows: 8, maxRows: 18 }} placeholder="127.0.0.1 localhost" />
            </Form.Item>
          )}
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
