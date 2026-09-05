# 项目环境使用规范

> 文档状态：有效
>
> 适用范围：FluxDNS 项目工具链、命令调用、构建物、依赖目录、缓存和工具安装边界
>
> 关联文档：[本地测试规范](local-testing.md)

本文档是 FluxDNS 项目级开发环境的权威使用规范。它负责约束项目工具链的来源、版本、调用方式和本地产物位置；本地测试配置、运行时数据和 DoH smoke test 流程见[本地测试规范](local-testing.md)。除非另有说明，Rust 命令从仓库根目录执行，Node.js 与 pnpm 命令从 `frontend/` 目录执行。

## 1. 项目工具链

项目共享工具链由仓库根目录的 `mise.toml` 声明，当前基线如下：

| 工具 | 项目版本 | 版本依据 |
| --- | --- | --- |
| Rust | `1.98.0` | `mise.toml` |
| Node.js | `26.8.1` | `mise.toml`、`frontend/package.json` 的 `engines` |
| pnpm | `11.25.0` | `mise.toml`、`frontend/package.json` 的 `packageManager` 和 `engines` |

前端依赖的具体解析结果以 `frontend/pnpm-lock.yaml` 为准。工具版本发生变化时，应先更新 `mise.toml` 以及直接受影响的版本声明和锁文件，再更新本规范；个人临时版本不写入项目配置。

开始构建或测试前，可从仓库根目录执行 `mise ls rust node pnpm --current`，并分别执行 `rustc --version`、`node --version` 和 `pnpm --version`，确认命令来自项目声明的工具链。必要时使用 `Get-Command rustc,node,pnpm -All` 检查实际命中路径，避免落到其他全局安装。

## 2. 命令调用规则

优先使用当前环境已激活且由项目 `mise.toml` 解析出的命令。只有命令未找到、shim 不可用或未解析到项目工具链时，才使用 `mise exec -- <command>` 桥接，并记录具体回退原因。

“无法直接执行”不包括命令自身返回的编译、依赖、类型检查、测试、构建或请求错误；这类错误应先按命令输出诊断，不应仅因业务失败就切换到另一套工具链。

### Rust

Rust 后端命令从仓库根目录执行，并通过 `--manifest-path backend/Cargo.toml` 指定 manifest：

```powershell
cargo fmt --manifest-path backend/Cargo.toml -- --check
cargo test --manifest-path backend/Cargo.toml
```

如直调失败且确认需要桥接，使用对应的 `mise exec -- <command>` 形式，例如：

```powershell
mise exec -- cargo test --manifest-path backend/Cargo.toml
```

### Node.js 与 pnpm

Node.js 与 pnpm 命令在 `frontend/` 目录执行。先直接调用项目环境中的 `node`、`pnpm`，不直接调用全局 `vite`、`vitest` 或 `tsc`：

```powershell
Set-Location frontend
node --version
pnpm --version
pnpm install --frozen-lockfile
pnpm run typecheck
pnpm run test
pnpm run build
```

前端脚本统一通过 `pnpm run <script>` 执行，依赖提供的 CLI 使用 `pnpm exec <command>`；不使用 `npm install` 替代 `pnpm install`。锁文件存在时必须使用 `pnpm install --frozen-lockfile`，避免验证过程改写 `frontend/pnpm-lock.yaml`。

如直调失败且确认需要桥接，仍在 `frontend/` 目录使用对应的形式，例如：

```powershell
mise exec -- pnpm install --frozen-lockfile
mise exec -- node --version
```

## 3. 构建物、依赖和缓存

构建、依赖和工具链缓存不得混入源码提交。当前仓库约定如下：

| 类型 | 本地目录 | 维护约定 |
| --- | --- | --- |
| Rust 后端构建物 | `backend/target/` | 已加入 `.gitignore` |
| Rust 工具链缓存 | `backend/.cargo-home/` | 由 `mise.toml` 固定，已加入 `.gitignore` |
| 前端依赖 | `frontend/node_modules/` | 已加入 `.gitignore` |
| 前端构建物 | `frontend/dist/` | 已加入 `.gitignore` |
| 发布二进制 | `deploy/` | 用于保存本地脚本或自动发布 workflow 生成的内嵌资源二进制；已加入 `.gitignore` |
| Node 编译缓存 | `frontend/.cache/node-compile-cache/` | 由 `mise.toml` 固定，已加入 `.gitignore` |
| npm 下载缓存 | `frontend/.cache/npm-cache/` | 由 `mise.toml` 固定，已加入 `.gitignore` |
| pnpm metadata cache | `frontend/.cache/pnpm-cache/` | 由 `mise.toml` 与 `frontend/pnpm-workspace.yaml` 固定，已加入 `.gitignore` |
| pnpm content-addressable store | `frontend/.cache/pnpm-store/` | 由 `mise.toml` 与 `frontend/pnpm-workspace.yaml` 固定，已加入 `.gitignore` |

新增前端技术栈或测试框架后，如产生其他固定目录，应在提交代码前同步补充 `.gitignore` 和本规范；不要通过 `git add -f` 强行提交构建物。个人配置、数据库、日志和本地测试运行时文件统一放在 `_fluxdns/`，其目录规则见[本地测试规范](local-testing.md)。

前端和后端的独立构建物不得重定向到 `deploy/`：`frontend/dist/` 由 Vite 保留，使用默认 feature 的后端 release 保留在 `backend/target/release/`，带 `webui-embed` 的当前平台 Cargo 构建物保留在 `backend/target/<triple>/release/`。发布脚本只复制当前平台最终文件到 `deploy/`，不移动或清理上述目录。

本地发布打包从仓库根目录执行 `pwsh -File script/package-embedded.ps1`。脚本先构建前端和默认 feature 的后端独立构建物，再检查 `webui-embed` feature 与当前 x86_64 平台 target，并生成一个内嵌 WebUI 的发布二进制；Windows 输出 `deploy/fluxdns-windows-x86_64.exe`，Linux 输出 `deploy/fluxdns-linux-x86_64`。两个平台应在各自原生环境执行，不要求单次调用具备跨平台 target/linker；脚本不自动安装工具链。

自动发布由 `.github/workflows/release.yml` 负责，只接收 `v*` tag，并在首个 job 中确认 tag 对应提交属于 `main`，且 tag、Cargo package 与前端 package 版本均和根目录 `VERSION` 一致。共享质量门禁依次完成前端测试/生产构建、Rust format、Clippy 和全 feature 测试；通过后，Windows x86_64、Linux x86_64 与原生 macOS ARM64 runner 并行消费同一份已测试 `frontend/dist`，生成 `fluxdns_<version>_windows_x86_64.zip`、`fluxdns_<version>_linux_x86_64.tar.gz` 和 `fluxdns_<version>_macos_arm64.tar.gz`。归档内包含平台二进制、根 README 和配置示例，Unix 归档保留可执行权限；最后一个 job 汇总三个 artifact、生成 `checksums.txt` 并创建 GitHub Release。GitHub 的 tag trigger 不能直接与 branch filter 做 AND 组合，因此 `main` 归属检查是 workflow 的第一项发布门禁，非 `main` 提交上的 tag 不会进入测试、构建或发布阶段。

根目录 `VERSION` 是项目当前发布版本的权威入口，仅保存一行不带 `v` 前缀的 SemVer。准备发布版本时，从 `main` 分支的仓库根目录执行 `pwsh -File script/set-version.ps1 <version>`。脚本接受 `0.2.0` 或 `v0.2.0` 形式，同步更新 `VERSION`、`backend/Cargo.toml`、`backend/Cargo.lock` 与 `frontend/package.json`，以 `chore(release): 发布 v<version>` 提交这四个文件，并为该提交创建本地 `v<version>` tag；已有同名 tag 或版本未变化时会在写入前停止。脚本默认检查 staged、unstaged 和 untracked 修改，只要工作树不干净，就会在写入前列出修改并停止。确认需要保留这些修改并继续时，显式传入 `-IgnoreUncommittedChanges`，例如 `pwsh -File script/set-version.ps1 0.2.0 -IgnoreUncommittedChanges`；该参数只跳过工作树保护，版本提交仍限定为上述四个路径，其他路径不会加入提交，但这四个文件中原有的修改会一并提交。脚本不会 push；检查提交和 tag 后，应依次推送 `main` 与对应的 `v<version>` tag，避免 tag workflow 在远端 `main` 尚未包含该提交时被门禁拒绝。

运行时使用 `pwsh -File script/dev.ps1 start -ConfigPath <path>`，配置路径为必填参数，不能省略或依赖脚本默认值；可用 `pwsh -File script/dev.ps1 status` 查看状态，或用 `pwsh -File script/dev.ps1 stop` 停止由脚本启动的进程。

## 4. 工具安装边界

- 禁止未经批准安装额外工具。
- 项目当前确实需要且本机缺失的工具（例如检查 SQLite 数据库所需的 SQLite CLI）可以自行安装，但必须确认当前 `mise` 支持管理该工具，并通过 `mise` 安装或切换版本。
- `mise` 不支持的工具、与当前项目任务无关的工具以及其他依赖工具，安装前必须获得明确批准。未经批准不得通过 `cargo install`、`npm install -g`、`pip install`、Scoop、WinGet、Chocolatey 或其他包管理器绕过该规则。
- 如果工具版本需要成为项目共享基线，应同步更新 `mise.toml`；个人临时工具不写入仓库配置。
- DoH smoke test 所需的 `doggo`、`curl` 等工具是否存在及缺失时的处理，遵循[本地测试规范](local-testing.md)，不得为了补齐单次测试而自行安装替代工具。

## 5. 环境核验与记录

环境安装、切换或排障后，至少核对以下项目：

1. `mise ls rust node pnpm --current` 显示的版本、来源和当前目录解析结果。
2. `rustc --version`、`node --version` 和 `pnpm --version` 的实际输出；必要时补充 `Get-Command rustc,node,pnpm -All` 的命中路径。
3. 构建或测试实际使用的命令、工具版本和 toolchain 来源；环境异常时记录是否使用了 `mise exec --` 桥接及具体原因。

不要把未执行的构建或测试命令描述为已通过；本地测试的配置路径、DoH 请求和结果记录按[本地测试规范](local-testing.md)执行。
