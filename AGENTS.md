# FluxDNS 项目级协作规范

本文档适用于 FluxDNS 仓库及其所有子目录。更具体的目录级 `AGENTS.md`（如后续新增）优先于本文档；用户明确要求优先于本文档和其他项目文档。

## 文档路由

开始修改前，按任务范围读取对应文档；涉及多个范围时同时读取相关文档：

| 任务范围 | 必读文档 |
| --- | --- |
| 本地配置、构建物、工具安装、DoH smoke test 和测试结果记录 | [`docs/standards/local-testing.md`](docs/standards/local-testing.md) |
| 规范文档索引及新增规范的归档位置 | [`docs/standards/README.md`](docs/standards/README.md) |
| 配置字段、路径解析、校验和迁移 | [`docs/configuration-reference.md`](docs/configuration-reference.md) |
| 后端总体架构、运行时边界和跨模块契约 | [`docs/backend-architecture.md`](docs/backend-architecture.md) |
| 后端模块实现 | 对应的 [`docs/backend-modules/`](docs/backend-modules/) 文档，并结合 [`docs/backend-development-plan.md`](docs/backend-development-plan.md) |
| 前端目录和前端工程约定 | [`frontend/README.md`](frontend/README.md) |

`docs/standards/` 用于存放跨模块、可执行的项目规范。新增规范文档时，应同步更新该目录的索引和本表，避免只新增文件而没有路由入口。

## 本地文件与路径

- 本地测试配置、规则、数据库、缓存、日志、临时证书及其他运行时文件统一放在仓库根目录的 `_fluxdns/`，不得散落到仓库根目录或源码目录。
- `_fluxdns/` 是本地专用目录，已加入 `.gitignore`，不得提交其中的个人配置或运行数据。
- 配置路径遵循两级基准：相对 `work.path` 以启动配置文件所在目录为基准；其他配置中的相对路径以解析后的 `work.path` 为基准。具体规则以 `docs/configuration-reference.md` 为准。

## 构建与验证

- 后端命令从仓库根目录执行，并通过 `--manifest-path backend/Cargo.toml` 指定 Rust manifest。
- Rust toolchain 使用项目 `mise.toml` 声明的版本；执行 `cargo`、`rustc`、`doggo` 等由 `mise` 管理的命令时，优先直接调用命令（例如 `cargo fmt --manifest-path backend/Cargo.toml -- --check`），仅当命令未找到、shim 不可用或未解析到项目工具链等无法直接执行的情况才使用 `mise exec --` 桥接，并记录回退原因；本地测试、DoH 工具和安装边界遵循 `docs/standards/local-testing.md`。
- 构建物和依赖目录必须由 `.gitignore` 覆盖。提交前检查 `git status --short`，不要使用 `git add -f` 提交本地产物。
- 文档或规则修改后至少检查 `git diff --check`，并按受影响范围执行最小充分验证；不要把未执行的测试描述为已通过。

## 工具安装规则（强制）

- 禁止未经批准安装额外工具。
- 项目当前确实需要且本机缺失的工具（例如检查 SQLite 数据库所需的 SQLite CLI）可以自行安装，但仅限于当前 `mise` 支持管理的工具，并必须通过 `mise` 安装或切换版本。
- `mise` 不支持的工具、与当前项目任务无关的工具以及其他依赖工具，安装前必须获得明确批准；不得用其他包管理器绕过这条规则。
- 如需把工具版本纳入项目共享基线，应同步更新 `mise.toml`；个人临时工具不得写入仓库配置。

## 变更边界

- 保持配置契约、文档路由和现有目录职责一致；行为变化必须同步更新直接受影响的文档、示例和测试说明。
- 优先做最小范围修改，不顺手重构无关代码或格式化整个仓库。
- 提交前审查真实 diff，精确暂存当前任务文件；未明确要求时不执行 push。
