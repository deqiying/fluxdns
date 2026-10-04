import { Alert, Button, Empty, Result, Skeleton, Space, Typography } from "antd";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";

interface PageStateProps {
  loading?: boolean;
  error?: unknown;
  empty?: boolean;
  emptyDescription?: string;
  onRetry?: () => void;
  compact?: boolean;
  hasData?: boolean;
}

export function PageState({
  loading,
  error,
  empty,
  emptyDescription = "暂无数据",
  onRetry,
  compact = false,
  hasData = false,
}: PageStateProps) {
  if (loading) {
    return <Skeleton active paragraph={{ rows: compact ? 2 : 6 }} />;
  }

  if (error) {
    const requestId = error instanceof ApiError ? error.requestId : undefined;
    const message = error instanceof ApiError && error.code === "OPERATION_BUSY"
      ? "配置读取暂时繁忙，请稍后重试。"
      : getSafeErrorMessage(error);
    if (hasData) return <Alert type="warning" showIcon title="刷新失败，当前显示上次读取的内容" description={`${message}${requestId ? ` 请求 ID：${requestId}` : ""}`} action={onRetry ? <Button size="small" onClick={onRetry}>重试</Button> : undefined} />;
    return (
      <Result
        status="warning"
        title={message}
        subTitle={requestId ? `请求 ID：${requestId}` : undefined}
        extra={onRetry ? <Button onClick={onRetry}>重试</Button> : undefined}
      />
    );
  }

  if (empty) {
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={emptyDescription} />;
  }

  return null;
}

export function InlineUnavailable({ reasonCode }: { reasonCode?: string }) {
  return (
    <Space orientation="vertical" size={0}>
      <Typography.Text type="secondary">暂不可用</Typography.Text>
      {reasonCode ? <Typography.Text className="reason-code">{reasonCode}</Typography.Text> : null}
    </Space>
  );
}
