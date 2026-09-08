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
pnpm run test:contract:v2
pnpm run test
pnpm run build
```

生成类型不手工修改。P0 [v2 目标契约](openapi/management-api-v2.yaml) 生成到独立 `generated-v2.ts`；dashboard/queries 仍使用 v1，`/system-runtime` 已使用 BC-23 的 v2 进程指标并复用 v1 基础信息，FC-02 配置公共层也显式选择 v2 client。后端 v2 命名空间目前只开放 BC-23 两个指标读端点，尚未成套切换正式配置路由。`generate:api` 同时更新当前与目标产物，`test:contract:v2` 单独校验跨端夹具。上述为操作命令，不是通过记录。内嵌打包、显式配置启动、版本与自动发布，以及浏览器/原生平台的现有验收边界，统一见[交付实现](../docs/implementation/delivery.md)。
