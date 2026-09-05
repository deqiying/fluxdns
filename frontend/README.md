# FluxDNS 前端

> 文档状态：有效
>
> 适用范围：前端工程最短开发入口与导航

React + TypeScript + Vite WebUI。实际认证、路由和页面接线见[前端实现](../docs/implementation/frontend/README.md)，设计见[前端架构](../docs/architecture/frontend.md)，字段以 [OpenAPI](openapi/management-api-v1.yaml) 为准。

## 开发

先按[环境规则](../docs/rules/environment-usage.md)确认项目 Node.js/pnpm 已就绪，不在本入口自动安装工具链。以下命令从仓库根目录进入前端后执行：

```powershell
Set-Location frontend
pnpm install --frozen-lockfile
pnpm run dev
```

开发代理指向本地 `127.0.0.1:8080` Management 服务；要使用契约 fixture，在启动 dev 前显式设置：

```powershell
$env:VITE_USE_MOCK_API = "true"
pnpm run dev
```

该变量只用于 DEV，mock 不等价于真实后端验收。

## 生成与验证

在 `frontend/` 内，修改 OpenAPI 后生成类型，再执行所需检查：

```powershell
pnpm run generate:api
pnpm run typecheck
pnpm run test
pnpm run build
```

生成类型不手工修改。上述为操作命令，不是通过记录。内嵌打包、显式配置启动、版本与自动发布的唯一说明见[交付实现](../docs/implementation/delivery.md)，剩余浏览器/原生平台验收见[v2 计划](../docs/plans/webui-v2-management-integration.md)。
