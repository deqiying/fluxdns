import type { ReactNode } from "react";
import { Alert, App, Button, Modal, Space, Typography } from "antd";
import { Save } from "lucide-react";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";

export interface ConfigFormModalProps {
  open: boolean;
  title: string;
  dirty: boolean;
  busy?: boolean;
  submitDisabled?: boolean;
  error?: unknown;
  children: ReactNode;
  onCancel: () => void;
  onSubmit: () => void;
}

export interface FormFieldError {
  name: string[];
  errors: string[];
}

/** 把 OpenAPI 字段路径转换为 Ant Design Form 可消费的定位信息。 */
export function formFieldErrors(error: unknown): FormFieldError[] {
  if (!(error instanceof ApiError)) return [];
  return error.fieldErrors.map((item) => ({
    name: item.path.split(/[/.]/).filter(Boolean),
    errors: [item.code],
  }));
}

export function ConfigFormModal({
  open,
  title,
  dirty,
  busy = false,
  submitDisabled = false,
  error,
  children,
  onCancel,
  onSubmit,
}: ConfigFormModalProps) {
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
      className="config-form-modal"
      open={open}
      title={title}
      width={720}
      centered
      destroyOnHidden
      keyboard={!busy}
      mask={{ closable: !busy }}
      onCancel={requestClose}
      footer={(
        <Space>
          <Button disabled={busy} onClick={requestClose}>取消</Button>
          <Button
            type="primary"
            icon={<Save size={16} aria-hidden="true" />}
            loading={busy}
            disabled={submitDisabled}
            onClick={onSubmit}
          >
            保存
          </Button>
        </Space>
      )}
    >
      {error ? (
        <Alert
          className="config-form-error"
          type="error"
          showIcon
          message={getSafeErrorMessage(error)}
          description={error instanceof ApiError && error.requestId
            ? <Typography.Text type="secondary">请求 ID：{error.requestId}</Typography.Text>
            : undefined}
        />
      ) : null}
      {children}
    </Modal>
  );
}
