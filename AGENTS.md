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
- Rust toolchain 使用项目 `mise.toml` 声明的版本；本地测试、DoH 工具和安装边界遵循 `docs/standards/local-testing.md`。
- 构建物和依赖目录必须由 `.gitignore` 覆盖。提交前检查 `git status --short`，不要使用 `git add -f` 提交本地产物。
- 文档或规则修改后至少检查 `git diff --check`，并按受影响范围执行最小充分验证；不要把未执行的测试描述为已通过。

## 变更边界

- 保持配置契约、文档路由和现有目录职责一致；行为变化必须同步更新直接受影响的文档、示例和测试说明。
- 优先做最小范围修改，不顺手重构无关代码或格式化整个仓库。
- 提交前审查真实 diff，精确暂存当前任务文件；未明确要求时不执行 push。
