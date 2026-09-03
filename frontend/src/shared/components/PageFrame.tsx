import type { ReactNode } from "react";
import { Flex, Space, Typography } from "antd";

interface PageFrameProps {
  title: string;
  description: string;
  meta?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
}

export function PageFrame({ title, description, meta, actions, children }: PageFrameProps) {
  return (
    <section className="page-frame">
      <Flex className="page-heading" justify="space-between" align="flex-start" gap={24} wrap>
        <Space orientation="vertical" size={4}>
          <Typography.Title level={2}>{title}</Typography.Title>
          <Typography.Paragraph type="secondary">{description}</Typography.Paragraph>
          {meta}
        </Space>
        {actions}
      </Flex>
      {children}
    </section>
  );
}
