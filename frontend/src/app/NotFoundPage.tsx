import { Button, Result } from "antd";
import { useNavigate } from "react-router-dom";

export function NotFoundPage() {
  const navigate = useNavigate();
  return (
    <Result
      status="404"
      title="页面不存在"
      subTitle="该地址不属于 FluxDNS 当前只读管理界面。"
      extra={<Button onClick={() => navigate("/dashboard")}>返回总览</Button>}
    />
  );
}
