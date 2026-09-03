import { Button, Empty, Result, Skeleton, Space, Typography } from "antd";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";

interface PageStateProps {
  loading?: boolean;
  error?: unknown;
  empty?: boolean;
  emptyDescription?: string;
  onRetry?: () => void;
  compact?: boolean;
}

export function PageState({
  loading,
  error,
  empty,
  emptyDescription = "暂无数据",
  onRetry,
  compact = false,
}: PageStateProps) {
  if (loading) {
    return <Skeleton active paragraph={{ rows: compact ? 2 : 6 }} />;
  }

  if (error) {
    const requestId = error instanceof ApiError ? error.requestId : undefined;
    return (
      <Result
        status="warning"
        title={getSafeErrorMessage(error)}
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
