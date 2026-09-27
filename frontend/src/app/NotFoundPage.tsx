import { Button, Result } from "antd";
import { useNavigate } from "react-router-dom";

export function NotFoundPage() {
  const navigate = useNavigate();
  return (
    <Result
      status="404"
      title={<h2>页面不存在</h2>}
      subTitle="该地址不属于 FluxDNS 管理后台。"
      extra={<Button type="primary" onClick={() => navigate("/dashboard")}>返回服务状态</Button>}
    />
  );
}
