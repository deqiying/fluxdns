# 构建、运行与发布实现

> 文档状态：有效
>
> 适用范围：前端生成/构建、内嵌打包、开发进程、版本脚本与 Release workflow 行为
>
> 最后核对：2026-09-09（P4 WS 依赖、内嵌调试构建与浏览器验证）
>
> 核对基线：`309f49bbd22dc725bd54ecf6d8cc213251b63773` 加本次 P4 文档工作树

## 工具与命令边界

版本由 [mise.toml](../../mise.toml) 与各 manifest 声明；工具来源、调用、安装审批和缓存位置唯一维护于[环境规则](../rules/environment-usage.md)。本文说明脚本真实行为，不授权安装、启动服务、提交 tag、push 或发布。

以下命令是操作入口示例，不是本轮执行记录。Rust/仓库脚本从根目录运行，pnpm 从 `frontend/` 运行；本地配置和运行文件遵循[本地测试规则](../rules/local-testing.md)。

## 前端与接口生成

[`frontend/package.json`](../../frontend/package.json) 定义 `dev`、`generate:api`、`typecheck`、`test` 和 `build`。`generate:api` 执行 `generate:api:v2`：唯一 [v2 OpenAPI](../../frontend/openapi/management-api-v2.yaml) 生成 [`generated-v2.ts`](../../frontend/src/shared/api/generated-v2.ts)。认证与业务共用该契约；生成文件不人工编辑，v1 schema/类型和旧页面 fixture 已删除。

`test:contract:v2` 用 Node 自带 test runner 运行 OpenAPI 3.1 schema 校验，与 Rust 消费同一个 [JSON 夹具](../../backend/tests/fixtures/management-v2.json)。它独立于 Vitest UI suite；`test` 不会自动包含该检查，契约变化必须额外执行。该检查不启动服务，不验证真实 HTTP/WS 或完整配置引用语义。

P0 依赖核定（2026-09-07）：`@redocly/ajv 8.11.2`、`js-yaml 4.3.1` 原已在锁文件作为间接依赖，本次按 D-08 显式加入 devDependencies，复用现有版本进行 JSON Schema/YAML 校验，不依赖隐式 hoist，不引入生产包。后端仅为既有 `ipnet 2.12.1`、`url 2.5.8` 开启 serde feature，避免复制 IP/URL 序列化逻辑，无 crate 版本升级。许可证及版本已按本地 package manifest 核对；前端两项为 MIT，后端两项为 MIT OR Apache-2.0。新增前端测试依赖不进入生产 bundle；Rust serde feature 可能增加编译产物，未测量其单独字节增量。未升级或安装工具链。

P1 文件事务依赖核定（2026-09-07）：既有 `windows-sys 0.61.2`（MIT OR Apache-2.0）仅增加 `Win32_Security` feature，用于创建文件时保留 owner/group/DACL 和禁止默认继承扩大访问；没有新增 crate 或升级锁定版本。OS 锁复用 Rust 标准库 `File::try_lock`，不添加锁库；未测量安全 API feature 的独立产物字节增量。Windows junction 回归调用项目既有 PowerShell 7，不安装测试工具。

P1 壳层依赖核定（2026-09-08）：新增锁定的 `lucide-react 1.41.0` 生产依赖，复用图稿采用的 Lucide 图标体系，供 12 个导航入口及折叠、移动菜单、登出控件使用，避免维护手绘 SVG。许可证为 ISC；registry 报告的完整包 unpacked size 为 32,023,893 bytes，实际只静态导入 16 个图标并由 Vite tree-shake，本轮不把完整包大小当成生产 bundle 增量，也未单独测量依赖增量。未增加构建脚本或工具链。

P1 指标采样依赖核定（2026-09-08）：未新增 crate 或升级锁定版本。Windows 复用既有 `windows-sys 0.61.2`（MIT OR Apache-2.0），增加 `Win32_Foundation`、`Win32_System_Diagnostics_ToolHelp`、`Win32_System_ProcessStatus` 和 `Win32_System_Threading` feature；Linux 使用 Rust 标准库读取 procfs。新增 feature 只参与对应目标编译，未测量其独立构建物字节增量；Linux 条件编译代码本批未实测。

P4 WS 依赖核定（2026-09-09）：为锁定的 `axum 0.8.9` 启用 `ws` feature，并将 `futures-util 0.3.34` 作为直接生产依赖，用于拆分有界 WS sink/stream；`tokio-tungstenite 0.29.0` 仅作为 dev-dependency 驱动真实 socket 测试。`axum` 与 `tokio-tungstenite` 为 MIT，`futures-util` 为 MIT OR Apache-2.0。未升级 Rust/Node/pnpm，也未新增前端图表库；服务状态图表使用现有 React/SVG。单独 feature/依赖体积增量未测量。

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

后端本机契约验证使用 [`test-backend-contracts.ps1`](../../script/test-backend-contracts.ps1)，不通过 `dev.ps1` 启停个人实例。该入口的 Local/Connections 模式、watchdog、证据目录及实际运行边界唯一维护于[后台服务](backend/background-services.md#契约验证运行入口)；不是构建/发布或目标环境性能验收入口。

独立跨平台负载 binary `contract-load` 通过 Cargo example 显式构建，不加入 FluxDNS 默认发布产物。构建、配置、停止语义和结果限制见[跨平台负载驱动](backend/background-services.md#跨平台负载驱动)；它不修改既有目标平台矩阵，也不自动安装交叉编译工具链。

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
| 前端构建 | `pnpm run build` | frontend package script | P4 工作树 typecheck、23 文件 99 项 Vitest、4 项 schema 与 Vite production build 通过 | 未运行 release 三阶段打包 |
| 本地打包 | package-embedded 三阶段 | 仓库根脚本 | 本轮静态检查顺序、产物和平台 gate | 未运行完整打包 |
| 显式启动/身份检查 | dev start/status/stop | debug embed binary + P4 ConfigV2 | 真实 start/status/stop、Bearer/UDP/SQLite/HTTP/WS 与两档浏览器 | 未作为 release binary 验收 |
| 版本提交/三平台发布 | set-version、release.yml | main + tag gates | 本轮静态 | 未创建提交/tag、push、Actions 或 Release |

历史记录：迁移前 v2 方案在 2026-09-04 报告 Windows x86_64 三阶段打包、发布物 SHA-256 对齐 target binary、配置 validate、移出外部 dist 后的 SPA/API HTTP smoke、dev start/status/stop、CSP/nosniff/cache/ETag/304，以及 in-app browser 的初始化深链接/表单/Console 检查。**这是原文报告，本轮未复核**；测试所用源码提交未完整记录，不能把本页核对基线视为当时测试基线。过时的 v2 方案已按用户要求移除，历史原文由 Git 追溯。

2026-09-08 Bearer 子项使用当批工作树执行前端构建、`cargo build --manifest-path backend/Cargo.toml --bin fluxdns --features webui-embed`，并启动独立 loopback 测试实例；真实 HTTP 和浏览器 Cookie/Network/Storage 证据见[Management 实现](backend/management.md#p1-bearer-业务鉴权2026-09-08)及[前端应用](frontend/application.md#p1-bearer-接线2026-09-08)。后续壳层子项执行 `pnpm run test` 9 文件 55 项和 `pnpm run build`，并用 Vite fixture 检查桌面、390×844、Drawer、上游 tab 与 Console；这不复核内嵌 binary、真实 v2 API、外部 HTTPS 代理、GitHub Actions 或 Linux/macOS 发布。

2026-09-08 P3 使用当批工作树执行 `pnpm run build` 与 `cargo build --manifest-path backend/Cargo.toml --bin fluxdns --features webui-embed`，由 debug 内嵌 binary 显式加载 `_fluxdns/p3-live/config.yaml`。真实 Bearer module/global HTTP、operation 轮询、文件持久化、UDP、SQLite、外改组合采用和两档浏览器视口证据见[Management 实现](backend/management.md#p1-配置事务与文件操作2026-09-08)与[前端应用](frontend/application.md#p3-联合验收2026-09-08)。该验证不等于 release 三阶段打包、HTTPS 反向代理、Linux/macOS、GitHub Actions 或发布授权。

2026-09-09 P4 使用当批工作树执行 `pnpm run test`、`test:contract:v2`、`build` 和 `cargo build --manifest-path backend/Cargo.toml --bin fluxdns --features webui-embed`，由 debug 内嵌 binary 显式加载 `_fluxdns/p4-live/config.yaml`。真实 HTTP/UDP/SQLite/WS smoke 返回指标 200、Cookie-only 401、DNS `NOERROR`、online push、断线 replay、协议 `fluxdns.v1` 和登出 4401；内嵌浏览器核对 Network/Storage、ticket subprotocol、实时 dashboard、稳定详情、1440×900 与 390×844 无页面级溢出。具体证据见[Management 实现](backend/management.md#p4-实时事件与断线补齐2026-09-09)与[前端应用](frontend/application.md#p4-共享实时连接2026-09-09)。这不等于 release 三阶段打包、HTTPS 反向代理、Linux/macOS、GitHub Actions、约 10 客户端或 2ms 性能验收。
