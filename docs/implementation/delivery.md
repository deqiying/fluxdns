# 构建、运行与发布实现

> 文档状态：有效
>
> 适用范围：前端生成/构建、内嵌打包、开发进程、版本脚本与 Release workflow 行为
>
> 最后核对：2026-09-05（manifest、脚本与 workflow 静态核对）
>
> 核对基线：`0f18d5b2ddf67625121fd7e0662e21723362565f`

## 工具与命令边界

版本由 [mise.toml](../../mise.toml) 与各 manifest 声明；工具来源、调用、安装审批和缓存位置唯一维护于[环境规则](../rules/environment-usage.md)。本文说明脚本真实行为，不授权安装、启动服务、提交 tag、push 或发布。

以下命令是操作入口示例，不是本轮执行记录。Rust/仓库脚本从根目录运行，pnpm 从 `frontend/` 运行；本地配置和运行文件遵循[本地测试规则](../rules/local-testing.md)。

## 前端与接口生成

[`frontend/package.json`](../../frontend/package.json) 定义 `dev`、`generate:api`、`typecheck`、`test` 和 `build`。`generate:api` 从 [OpenAPI](../../frontend/openapi/management-api-v1.yaml) 生成 [`shared/api/generated.ts`](../../frontend/src/shared/api/generated.ts)，生成文件不人工编辑。

[`vite.config.ts`](../../frontend/vite.config.ts) 在开发时把 `/api` 代理到 `http://127.0.0.1:8080`；浏览器仍请求同源相对路径。`VITE_USE_MOCK_API=true` 只在 DEV bootstrap 启用 MSW，生产构建不携带 mock worker 或 source map。完整生成与验证命令见[前端 README](../../frontend/README.md)。

## 本地内嵌打包

从仓库根目录、准备好项目工具链后执行：

```powershell
pwsh -File script/package-embedded.ps1
```

[`package-embedded.ps1`](../../script/package-embedded.ps1) 先检查 PowerShell、当前 OS/架构和必需命令，只支持 Windows/Linux x86_64；不支持本地 macOS/跨架构调用。之后按三阶段执行：

1. `pnpm install --frozen-lockfile` 与 `pnpm run build`，保留 `frontend/dist/`。
2. 默认 feature 的 Cargo locked release，保留 `backend/target/release/`。
3. 检查 `webui-embed` 与当前平台 target，构建 target-specific release，将最终 binary 复制到 `deploy/`。

| 平台 | 内嵌 target 构建物 | 本地发布物 |
| --- | --- | --- |
| Windows x86_64 | `backend/target/x86_64-pc-windows-msvc/release/fluxdns.exe` | `deploy/fluxdns-windows-x86_64.exe` |
| Linux x86_64 | `backend/target/x86_64-unknown-linux-gnu/release/fluxdns` | `deploy/fluxdns-linux-x86_64` |

脚本不移动或重定向独立构建物，不自动安装 target/linker。最终 feature/target 检查失败时前两阶段产物仍保留；不能因此把已有旧发布物当成本次成功产物。脚本先检查 dist/index.html，[`management/assets.rs`](../../backend/src/management/assets.rs) 的 RustEmbed 派生在 feature 启用时内嵌资源并在启动时检查 index；默认非 embed 构建仍可提供 Management API，但不含 SPA。

## 开发进程管理

[`dev.ps1`](../../script/dev.ps1) 的 `start` 必须显式提供 `-ConfigPath`，可用 `-BinaryPath` 指定 binary；不回退到 `_fluxdns/config.yaml` 或 CLI 默认配置。以下示例要求本地配置和当前平台发布物已经存在：

```powershell
pwsh -File script/dev.ps1 start -ConfigPath ./_fluxdns/config.yaml
pwsh -File script/dev.ps1 status
pwsh -File script/dev.ps1 stop
```

状态保存在 `_fluxdns/dev-process.json`，stdout/stderr 在 `_fluxdns/logs/dev.stdout.log` 与 `dev.stderr.log`。`status` 返回 0 表示运行、3 表示未运行；参数错误、损坏状态或无法安全核验进程为非零错误。

脚本根据 PID、启动时间和 executable 身份检查进程，`stop` 只停止匹配进程，不仅凭陈旧 PID 操作其他进程。不要手工把共享或生产进程写入状态文件。

## 版本与自动发布

[`VERSION`](../../VERSION) 是发布版本权威，保存不带 `v` 的单行 SemVer。[`set-version.ps1`](../../script/set-version.ps1) 在 `main` 接受带或不带 `v` 的版本参数，同步 `VERSION`、Cargo manifest/lock 与前端 package，然后以 `chore(release): 发布 v<version>` 提交这四个文件并创建本地 tag。该脚本有 Git 写副作用，只有明确准备发布版本时才运行。

已有 tag、版本未变或默认工作树不干净时停止。`-IgnoreUncommittedChanges` 只绕过工作树保护，不扩大四文件提交范围，但这四文件中已有修改会一并进入版本提交。脚本不会 push。另行获准推送时应先推 `main`，再推该 tag，确保远端 main 已包含版本提交。

[`.github/workflows/release.yml`](../../.github/workflows/release.yml) 由 `v*` tag 触发：

1. 校验 tag 提交属于 `main`，且 tag、VERSION、Cargo 与前端 package 版本一致。
2. 共享门禁执行前端测试/构建、Rust fmt/Clippy/全 feature 测试。
3. Windows x86_64、Linux x86_64、macOS ARM64 原生 runner 消费同一份已测试前端产物，分别构建内嵌 binary。
4. 归档平台 binary、根 README 与配置示例，Unix 保留 executable 权限；汇总 artifacts 与 `checksums.txt` 后创建 GitHub Release。

归档名为 `fluxdns_<version>_windows_x86_64.zip`、`fluxdns_<version>_linux_x86_64.tar.gz`、`fluxdns_<version>_macos_arm64.tar.gz`。workflow 存在不证明 Actions 已跑通；本地 x86_64 脚本也不等同三平台自动发布。

## 证据与验收边界

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 本地打包 | package-embedded 三阶段 | 仓库根脚本 | 本轮静态检查顺序、产物和平台 gate | 未运行 build/打包 |
| 显式启动/身份检查 | dev start/status/stop | 本地发布 binary | 本轮静态 | 未启动或停止服务 |
| 版本提交/三平台发布 | set-version、release.yml | main + tag gates | 本轮静态 | 未创建提交/tag、push、Actions 或 Release |

历史记录：迁移前 v2 方案在 2026-09-04 报告 Windows x86_64 三阶段打包、发布物 SHA-256 对齐 target binary、配置 validate、移出外部 dist 后的 SPA/API HTTP smoke、dev start/status/stop、CSP/nosniff/cache/ETag/304，以及 in-app browser 的初始化深链接/表单/Console 检查。**这是原文报告，本轮未复核**；测试所用源码提交未完整记录，不能把本页核对基线视为当时测试基线。过时的 v2 方案已按用户要求移除，历史原文由 Git 追溯。

本页没有真实浏览器 Cookie/Network/Storage、GitHub Actions 实跑、Linux/macOS 原生发布与完整故障矩阵的新执行证据；删除过时计划不等于这些场景已经验证通过。
