# 构建、运行与发布实现

> 文档状态：有效
>
> 适用范围：前端生成/构建、内嵌打包、开发进程、版本脚本与 Release workflow 行为
>
> 最后核对：2026-09-10（Release workflow 的 WebUI 产物依赖与代理页测试超时修复）
>
> 核对基线：`6e4c50d` 与本次修复；本轮核对 Release workflow 的 WebUI 产物依赖和代理页测试，历史运行结果按原日期和基线解释

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

[`.github/workflows/release.yml`](../../.github/workflows/release.yml) 由 `v*` tag 触发，流程按依赖拆成 `prepare`、`frontend`、`rust-quality`、`create-release`、`build` 和 `finalize-release`：

1. `prepare` 只校验 tag 提交属于 `main`、工具版本和 tag/VERSION/Cargo/前端 package 版本，不再为元数据校验提前安装 Rust 或 Node。
2. `frontend` 依赖 `prepare`，完成 pnpm 安装、测试和构建后上传 `webui-dist`。`rust-quality` 依赖 `prepare` 和 `frontend`，在 Clippy/测试前把同一 artifact 下载到 `frontend/dist/`：`--all-features` 会启用 `webui-embed`，RustEmbed 在编译时就需要真实 WebUI，不能与该产物的生成完全并行。pnpm 依赖仍只安装一次，WebUI 仍只构建一次，Rust 质量门禁和所有平台共用同一产物。
3. Rust job 使用按 runner OS 和 `Cargo.lock` 哈希命名的 `actions/cache` 复用 Cargo registry/git 源；平台 target 的 `target/` 仍按 target 独立缓存，因为不同 OS/target 的编译产物不可安全混用。Linux 的 GNU 与 musl job 可以复用同一份依赖源缓存，Windows/macOS 仍使用各自 runner 的缓存空间。
4. `create-release` 在两类质量门禁都成功后创建 draft Release。四项 `build` matrix 只依赖共享门禁和这个草稿，`max-parallel: 4`、`fail-fast: false` 允许 Windows x86_64、Linux x86_64、OpenWrt x86_64、macOS ARM64 同时执行；实际调度仍受 GitHub runner 可用性与账户并发配额限制。
5. 每个平台完成打包和 `--version` 校验后立即通过 `gh release upload` 上传自己的 archive，使用 `--clobber` 支持失败重跑。平台上传不再等待其他二进制完成，因此 Release 草稿可以逐步看到已完成的资产；OpenWrt 仍在 Ubuntu runner 上使用 musl，其他三项保持各自现有 target。
6. `finalize-release` 只在四项 matrix 全部成功后下载四个 archive，生成并上传 `checksums.txt`，再把 draft Release 发布。也就是说，上传资产不必等待四个平台全部完成，但正式发布仍保留“四个平台完整且校验和齐全”的门禁。

| 平台 | Rust target | Release 归档 |
| --- | --- | --- |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `fluxdns_<version>_windows_x86_64.zip` |
| Linux x86_64（glibc） | `x86_64-unknown-linux-gnu` | `fluxdns_<version>_linux_x86_64.tar.gz` |
| OpenWrt x86_64（musl 静态链接） | `x86_64-unknown-linux-musl` | `fluxdns_<version>_openwrt_x86_64.tar.gz` |
| macOS ARM64 | `aarch64-apple-darwin` | `fluxdns_<version>_macos_arm64.tar.gz` |

OpenWrt 构建项在临时 Ubuntu runner 安装 `musl-tools` 与 `binutils`，为 `cc` 指定 `CC_x86_64_unknown_linux_musl=musl-gcc`，并为 Cargo 指定对应 target 的 linker；现有 SQLite bundled 与 ring C 代码随该工具链构建，不增加 Rust 依赖。该项显式启用 `-C target-feature=+crt-static`，打包前通过 `readelf` 拒绝 ELF `INTERP` 和动态 `NEEDED` 依赖，再复用公共的 `--version` 检查与打包流程。OpenWrt 安装端应选择 `_openwrt_x86_64.tar.gz`，不能回退使用 GNU/Linux 包；归档中的程序仍命名为 `fluxdns`。

上述工具安装属于该 CI 构建项；本地工具安装仍遵循[环境规则](../rules/environment-usage.md)，本地 `package-embedded.ps1` 仍只支持原有 Windows/Linux x86_64。workflow 配置、静态链接检查与版本检查不等同 OpenWrt 上的真实 DNS/DoH 验收；新增平台尚待 GitHub Actions 构建和目标设备运行验证。

## 证据与验收边界

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 前端构建 | package scripts | 唯一 v2 类型生成与 Vite production build | 本轮 Windows：24 文件 98 项 Vitest、typecheck/build 通过 | 不以 mock 代替真实后端；本轮未重跑独立 schema suite |
| 本地打包 | package-embedded 三阶段 | 仓库根脚本、Windows target 与 deploy | 完整三阶段成功，deploy 与 target SHA-256 相同 | 未执行 Actions/Linux/macOS 发布 |
| Release 多平台发布 | release.yml 的 WebUI artifact 依赖、草稿 Release 与四项 matrix | Rust 质量门禁和平台构建复用一次生成的 WebUI；平台完成即上传，四项齐全后生成 checksums 并发布 | 本轮 YAML/DAG 检查、Windows 全 feature Clippy 和 Cargo 测试通过（795 项通过、4 项忽略） | 未执行真实 runner 调度、musl 编译、GitHub Release API 或 OpenWrt 设备运行 |
| 显式启动/身份检查 | dev start/status/stop | 最终 release embed 与独立 ConfigV2 | 新目录启动、受控重启、文件摘要不变、FDCS 恢复及旧分片 ID 可读 | 原生触控和外部 HTTPS 代理限制见联合验收 |
| 本地 HTTP/WS 验收 | test-webui-http.mjs、test-webui-events.ps1 | loopback 夹具与管理账号 | 四种 DNS 请求、配置/文件/安全、WS replay 与撤销 | 仅使用独立测试配置，不访问生产或公网 |
| 版本与远端发布 | set-version、release.yml | main + tag gates | 本轮不执行 | 没有 tag、push、Actions 或 Release 授权 |

当前命令、运行产物、29 图映射、E2E 矩阵与平台边界以 [WebUI 联合验收](webui-acceptance.md) 为准。本地脚本的夹具和凭据准备遵循[本地测试规范](../rules/local-testing.md#webui-真实-httpws-联合验收)；测试报告和密码留在忽略目录，不复制进 Git。历史批次日志由 Git 与原任务记录追溯，不把旧 debug 样本当作当前 release 的证明。
