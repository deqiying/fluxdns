import { useEffect, useMemo, useState } from "react";
import {
  Button,
  Form,
  Input,
  Segmented,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
  type TableColumnsType,
} from "antd";
import { Pencil, Plus, Search } from "lucide-react";
import type { components } from "@/shared/api/generated-v2";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { ConfigStateSummary } from "@/shared/components/ConfigStateSummary";
import { PageFrame } from "@/shared/components/PageFrame";
import { PageState } from "@/shared/components/PageState";
import { configStateEditable, useConfigChangeMutation, useConfigModule } from "@/shared/config/hooks";

type Schemas = components["schemas"];
type Outbound = Schemas["Outbound"];

interface ProxyFormValues {
  name: string;
  secretKind: "env" | "file";
  secretValue: string;
}

export function ProxiesPage() {
  const query = useConfigModule("outbound");
  const mutation = useConfigChangeMutation("outbound");
  const [form] = Form.useForm<ProxyFormValues>();
  const [editing, setEditing] = useState<Outbound | "create" | null>(null);
  const [dirty, setDirty] = useState(false);
  const [search, setSearch] = useState("");
  const proxies = useMemo(() => query.data?.values.flatMap((item) =>
    item.module === "outbound" ? [item.value] : []) ?? [], [query.data]);
  const visible = proxies.filter((item) =>
    `${item.name} ${"env" in item.proxy_url ? item.proxy_url.env : item.proxy_url.file}`
      .toLocaleLowerCase()
      .includes(search.trim().toLocaleLowerCase()));

  useEffect(() => {
    if (!editing) return;
    if (editing === "create") {
      form.setFieldsValue({ name: "", secretKind: "env", secretValue: "" });
    } else {
      form.setFieldsValue({
        name: editing.name,
        secretKind: "env" in editing.proxy_url ? "env" : "file",
        secretValue: "env" in editing.proxy_url ? editing.proxy_url.env : editing.proxy_url.file,
      });
    }
    setDirty(false);
    mutation.reset();
  }, [editing, form]);

  const columns: TableColumnsType<Outbound> = [
    { title: "名称", dataIndex: "name", width: 240, render: (value: string) => <Typography.Text strong>{value}</Typography.Text> },
    { title: "类型", width: 120, render: () => <Tag>SOCKS5</Tag> },
    {
      title: "SecretRef",
      render: (_, item) => "env" in item.proxy_url
        ? <Typography.Text code>env:{item.proxy_url.env}</Typography.Text>
        : <Typography.Text code>file:{item.proxy_url.file}</Typography.Text>,
    },
    {
      title: "引用",
      width: 110,
      render: (_, item) => query.data?.references.filter((reference) => reference.to_name === item.name).length ?? 0,
    },
    {
      title: "操作",
      width: 80,
      align: "center",
      render: (_, item) => (
        <Tooltip title="编辑代理">
          <Button type="text" aria-label={`编辑代理 ${item.name}`} icon={<Pencil size={17} />} onClick={() => setEditing(item)} />
        </Tooltip>
      ),
    },
  ];

  const submit = async () => {
    if (!query.data || !editing) return;
    const values = await form.validateFields();
    const value: Outbound = {
      name: values.name,
      type: "socks5",
      proxy_url: values.secretKind === "env" ? { env: values.secretValue } : { file: values.secretValue },
    };
    const change: Schemas["ConfigChange"] = editing === "create"
      ? { module: "outbound", change: { action: "create", value } }
      : { module: "outbound", change: { action: "update", original_name: editing.name, value } };
    try {
      const operation = await mutation.mutateAsync({ change, state: query.data.state });
      if (operation) setEditing(null);
    } catch {
      // mutation.error 由表单容器呈现，草稿保持不变。
    }
  };

  return (
    <PageFrame
      title="代理配置"
      description="管理供上游与远程规则使用的 SOCKS5 SecretRef；实际凭据不会进入 WebUI。"
      meta={query.data ? <ConfigStateSummary state={query.data.state} /> : undefined}
      actions={(
        <Button
          type="primary"
          icon={<Plus size={17} />}
          disabled={!query.data || !configStateEditable(query.data.state)}
          onClick={() => setEditing("create")}
        >
          添加代理
        </Button>
      )}
    >
      <PageState loading={query.isLoading} error={query.error} onRetry={() => void query.refetch()} />
      {query.data ? (
        <div className="config-module-content">
          <div className="config-table-toolbar">
            <Input
              allowClear
              value={search}
              prefix={<Search size={16} aria-hidden="true" />}
              placeholder="搜索代理名称或 SecretRef"
              onChange={(event) => setSearch(event.target.value)}
            />
          </div>
          <Table
            rowKey="name"
            columns={columns}
            dataSource={visible}
            pagination={{ pageSize: 20, hideOnSinglePage: true }}
            scroll={{ x: 760 }}
            locale={{ emptyText: search ? "没有匹配的代理" : "尚未配置代理" }}
          />
        </div>
      ) : null}
      <ConfigFormModal
        open={editing !== null}
        title={editing === "create" ? "添加代理" : "编辑代理"}
        dirty={dirty}
        busy={mutation.isPending}
        error={mutation.error}
        onCancel={() => setEditing(null)}
        onSubmit={() => void submit()}
      >
        <Form form={form} layout="vertical" requiredMark="optional" onValuesChange={() => setDirty(true)}>
          <Form.Item name="name" label="名称" rules={[{ required: true }, { max: 128 }]}>
            <Input autoComplete="off" />
          </Form.Item>
          <Form.Item name="secretKind" label="SecretRef 来源" rules={[{ required: true }]}>
            <Segmented block options={[{ label: "环境变量", value: "env" }, { label: "文件", value: "file" }]} />
          </Form.Item>
          <Form.Item noStyle shouldUpdate={(previous, current) => previous.secretKind !== current.secretKind}>
            {({ getFieldValue }) => (
              <Form.Item
                name="secretValue"
                label={getFieldValue("secretKind") === "file" ? "引用文件" : "环境变量"}
                rules={[{ required: true }, { max: 4096 }]}
              >
                <Input autoComplete="off" placeholder={getFieldValue("secretKind") === "file" ? "./secrets/proxy.txt" : "PROXY_URL"} />
              </Form.Item>
            )}
          </Form.Item>
          <Space size={6}>
            <Typography.Text type="secondary">仅保存引用位置，不显示解析后的 URL、用户名、密码或令牌。</Typography.Text>
          </Space>
        </Form>
      </ConfigFormModal>
    </PageFrame>
  );
}
