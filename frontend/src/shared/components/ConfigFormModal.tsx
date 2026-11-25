import { useEffect, useRef, useState, type ReactNode } from "react";
import { Alert, App, Button, Modal, Space, type FormInstance } from "antd";
import { Save } from "lucide-react";
import { ApiError, getSafeErrorMessage, type ApiFieldError } from "@/shared/api/errors";
import type { ConfigModule } from "@/shared/config/api";

type FieldPath = (string | number)[];

interface ConfigFormFeedbackProps<Values> {
  open: boolean;
  error?: unknown;
  configDigest?: string;
  moduleDigest?: string;
  form?: FormInstance<Values>;
  module?: ConfigModule;
  fieldAliases?: Readonly<Record<string, string>>;
  mapField?: (field: ApiFieldError, path: FieldPath) => FieldPath | undefined;
}

export interface ConfigFormModalProps<Values> extends ConfigFormFeedbackProps<Values> {
  title: string;
  dirty: boolean;
  busy?: boolean;
  /** 仅供内容较长的专用表单启用更宽的可滚动布局。 */
  layout?: "default" | "wide";
  children: ReactNode;
  onCancel: () => void;
  onSubmit: () => void;
}

/** 兼容校验配置路径和请求解码路径。表单未注册的路径保留在顶部诊断中。 */
function configFieldPath(path: string, module: ConfigModule, aliases?: Readonly<Record<string, string>>): FieldPath {
  const parts = path.replace(/\[(\d+)\]/g, ".$1").split(/[/.]/).filter(Boolean);
  if (parts[0] === "changes" && parts[2] === "change") parts.splice(0, parts[3] === "value" ? 4 : 3);
  else if (parts[0] === module) parts.splice(0, /^\d+$/.test(parts[1] ?? "") ? 2 : 1);
  const indexed = /^\d+$/.test(parts.at(-1) ?? "");
  const alias = aliases?.[parts.join(".")] ?? (indexed ? aliases?.[parts.slice(0, -1).join(".")] : undefined);
  return alias ? [alias] : parts.map((part) => /^\d+$/.test(part) ? Number(part) : part);
}

/** 弹窗和客户端抽屉共用字段错误与非阻断更新提示；摘要只用于提醒，不参与写入。 */
export function ConfigFormFeedback<Values>({ open, error, configDigest, moduleDigest, form, module, fieldAliases, mapField }: ConfigFormFeedbackProps<Values>) {
  const openingDigests = useRef<{ configDigest?: string; moduleDigest?: string } | null>(null);
  const [feedback, setFeedback] = useState<{ error: unknown; text?: string }>();
  const configChanged = open && openingDigests.current && (
    (configDigest && configDigest !== (openingDigests.current.configDigest ?? openingDigests.current.moduleDigest))
    || (moduleDigest && moduleDigest !== (openingDigests.current.moduleDigest ?? openingDigests.current.configDigest))
  );

  useEffect(() => {
    if (!open) openingDigests.current = null;
    else if (!openingDigests.current && (configDigest || moduleDigest)) openingDigests.current = { configDigest, moduleDigest };
  }, [open, configDigest, moduleDigest]);

  useEffect(() => {
    if (!open || !error) return;
    if (!form || !module || !(error instanceof ApiError) || !["VALIDATION_FAILED", "INVALID_ARGUMENT"].includes(error.code)) {
      setFeedback({ error, text: getSafeErrorMessage(error) });
      return;
    }
    const registered = new Set(form.getFieldsError().map(({ name }) => JSON.stringify(name)));
    const fields = new Map<string, { name: FieldPath; errors: string[] }>();
    const unmatched = error.fieldErrors.filter((field) => {
      const path = configFieldPath(field.path, module, fieldAliases);
      let name = mapField?.(field, path) ?? path;
      // 单个标签出错时，错误标记在其数组输入控件上。
      if (typeof name.at(-1) === "number" && !registered.has(JSON.stringify(name))) name = name.slice(0, -1);
      const key = JSON.stringify(name);
      if (!registered.has(key)) return true;
      const previous = fields.get(key)?.errors ?? [];
      fields.set(key, { name, errors: [...previous, field.code] });
      return false;
    });
    if (fields.size) form.setFields([...fields.values()] as Parameters<FormInstance<Values>["setFields"]>[0]);
    setFeedback({ error, text: !fields.size || unmatched.length || error.diagnostic
      ? getSafeErrorMessage(error, undefined, unmatched) : undefined });
  }, [open, error, form, module, fieldAliases, mapField]);

  const text = error && feedback?.error === error ? feedback?.text : undefined;
  return (
    <>
      {configChanged ? <Alert className="config-form-error" type="info" showIcon title="配置已更新，保存时将以当前内容替换本条目" /> : null}
      {text ? <Alert className="config-form-error" type="error" showIcon title={text} /> : null}
    </>
  );
}

export function ConfigFormModal<Values>({
  open,
  title,
  dirty,
  busy = false,
  layout = "default",
  children,
  onCancel,
  onSubmit,
  ...feedback
}: ConfigFormModalProps<Values>) {
  const { modal } = App.useApp();
  const requestClose = () => {
    if (busy) return;
    if (!dirty) {
      onCancel();
      return;
    }
    modal.confirm({
      title: "放弃未保存的修改？",
      content: "当前草稿尚未保存。",
      okText: "放弃修改",
      cancelText: "继续编辑",
      okButtonProps: { danger: true },
      onOk: onCancel,
    });
  };

  return (
    <Modal
      className={`config-form-modal${layout === "wide" ? " config-form-modal-wide" : ""}`}
      open={open}
      title={title}
      width={layout === "wide" ? 900 : 720}
      centered
      destroyOnHidden
      keyboard={!busy}
      closable={!busy}
      mask={{ closable: !busy }}
      onCancel={requestClose}
      footer={(
        <Space>
          <Button disabled={busy} onClick={requestClose}>取消</Button>
          <Button
            type="primary"
            icon={<Save size={16} aria-hidden="true" />}
            loading={busy}
            disabled={busy}
            onClick={onSubmit}
          >
            保存
          </Button>
        </Space>
      )}
    >
      <ConfigFormFeedback open={open} {...feedback} />
      {children}
    </Modal>
  );
}
