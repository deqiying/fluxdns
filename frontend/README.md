# FluxDNS 前端

> 文档状态：有效
>
> 适用范围：`frontend/` 工程入口、目录边界与前端开发导航

`frontend/` 是 FluxDNS 前端代码的独立主目录。

当前已实现 React + TypeScript + Vite 的只读 WebUI 工程、首次初始化、Cookie session、应用壳、页面路由、OpenAPI v1 类型和 MSW contract fixtures。后端 Management API 与 `webui-embed` 已接入；默认开发模式通过 Vite 将 `/api` 代理到 `127.0.0.1:8080`，也可以显式启用本地 mock transport 独立查看页面。

架构边界和实施状态见[前端架构设计](../docs/frontend/architecture.md)与[前端开发方案](../docs/frontend/development-plan.md)，接口字段以 [`openapi/management-api-v1.yaml`](openapi/management-api-v1.yaml) 为权威。前端实现保持在本目录内，不把仓库根目录作为前端工程目录。

## 环境和缓存

仓库根 `mise.toml` 固定 Node.js 26.8.1 和 pnpm 11.25.0。依赖、构建物和缓存分别位于 `node_modules/`、`dist/`、`.cache/`，均已由仓库 `.gitignore` 忽略；详细约定见[项目环境使用规范](../docs/standards/environment-usage.md)。

```powershell
mise install
Set-Location frontend
node --version
pnpm --version
pnpm install --frozen-lockfile
```

## 开发

连接本地 Management API：

```powershell
pnpm run dev
```

使用受跟踪的 contract fixtures 独立预览；mock 仅在 Vite development mode 生效，生产构建不会复制 `mockServiceWorker.js`：

```powershell
$env:VITE_USE_MOCK_API = "true"
pnpm run dev
```

开发代理固定将 `/api/*` 转发到 `http://127.0.0.1:8080`；浏览器代码始终使用同源相对路径，不读取任意生产 `baseURL`。

## 生成与验证

修改 OpenAPI schema 后重新生成 TypeScript 类型：

```powershell
pnpm run generate:api
```

提交前执行：

```powershell
pnpm run typecheck
pnpm run test
pnpm run build
```

`pnpm run test` 使用 Node 26 时可能输出 Node experimental localStorage warning；测试环境会用内存 `Storage` polyfill 隔离该能力，并断言应用未写入 `localStorage` 或 `sessionStorage`。

## 发布打包

从仓库根目录执行打包脚本：

```powershell
pwsh -File script/package-embedded.ps1
```

脚本按三个阶段执行：先生成并保留 `frontend/dist/`，再使用默认 feature 生成 `backend/target/release/` 的后端独立构建物，最后只为当前 x86_64 平台构建 `webui-embed` binary，并复制为 `deploy/fluxdns-windows-x86_64.exe` 或 `deploy/fluxdns-linux-x86_64`。双平台发布由 Windows x86_64 与 Linux x86_64 runner 各执行一次，不要求单台开发机准备另一平台的 target/linker。

`webui-embed` feature 与当前平台 target 的检查发生在前两阶段之后；检查或最终构建失败时，已经生成的前后端独立构建物仍会保留，但不会产生当前平台的新发布物。脚本不会移动或重定向 `frontend/dist/`、`backend/target/`，也不会自动安装 target/linker。发布二进制的启动、状态查看和停止入口及必填配置参数见[项目环境使用规范](../docs/standards/environment-usage.md)和[整合方案](../docs/plans/webui-v2-management-integration.md#103-开发服务管理脚本)。
