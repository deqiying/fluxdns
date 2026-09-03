# 本地测试规范

> 文档状态：有效
>
> 适用范围：FluxDNS 本地配置、运行时文件、构建物、工具和 DoH smoke test 管理

本文档约定 FluxDNS 在本地开发和验证时的文件位置、构建物管理、工具使用和 DoH smoke test 方式。除非另有说明，命令均从仓库根目录执行。

## 1. 本地目录约定

仓库根目录下的 `_fluxdns/` 是本地测试专用的工作目录。配置文件、规则文件、数据库、缓存、日志、临时证书和其他运行时生成文件都放在该目录或其子目录中，不要把这些文件散落在仓库根目录或代码目录中。

推荐的目录结构如下：

```text
_fluxdns/
├── config.yaml
├── data/
│   └── fluxdns.sqlite3
├── logs/
│   └── fluxdns.log
└── rules/
    └── hosts.txt
```

本地配置从仓库根目录的 `config-example.yaml` 复制后修改，不直接修改示例文件。例如：

```powershell
New-Item -ItemType Directory -Force _fluxdns | Out-Null
Copy-Item config-example.yaml _fluxdns/config.yaml
```

本地配置通常将 `work.path` 设为 `./`。配置路径按两级基准解析：

1. 启动配置文件的相对路径先相对于进程启动时的当前目录解析；配置文件所在目录作为 `config_dir`。
2. 相对 `work.path` 再相对于 `config_dir` 解析，得到 `resolved_work_path`。
3. 其他配置中的相对路径统一相对于 `resolved_work_path` 解析。

因此，配置文件为 `_fluxdns/config.yaml` 且 `work.path: ./` 时，`resolved_work_path` 是 `_fluxdns`；`database.path: ./data/fluxdns.sqlite3` 的实际路径是 `_fluxdns/data/fluxdns.sqlite3`，而不是进程当前目录下的 `data/fluxdns.sqlite3`。完整字段规则见 [配置参考](../backend/configuration-reference.md)。

`_fluxdns/` 已加入 `.gitignore`，其中内容默认不应被提交。需要长期维护、可复现并纳入版本控制的测试夹具，应放在专门的受跟踪目录中，不要依赖个人 `_fluxdns/` 内容。

## 2. 构建物与 Git 管理

构建、依赖和覆盖率等本地产物不得混入源码提交。当前仓库约定如下：

| 类型 | 本地目录 | Git 约定 |
| --- | --- | --- |
| Rust 后端构建物 | `backend/target/` | 已加入 `.gitignore` |
| Rust 工具链缓存 | `backend/.cargo-home/` | 已加入 `.gitignore` |
| 前端依赖 | `frontend/node_modules/` | 已加入 `.gitignore` |
| 前端构建物 | `frontend/dist/` | 已加入 `.gitignore` |
| 本地配置和运行时数据 | `_fluxdns/` | 已加入 `.gitignore` |

新增前端技术栈或测试框架后，如产生其他固定目录，应在提交代码前同步补充 `.gitignore` 和本规范；不要通过 `git add -f` 强行提交构建物。提交前检查 `git status --short`，确认没有配置、数据库、日志或构建目录进入暂存区。

## 3. 工具和安装边界

优先使用当前环境已有的命令和项目声明的工具链。Rust 后端从仓库根目录执行时使用项目 `mise.toml` 声明的 Rust 1.98.0。对于由 `mise` 管理的工具（如 `cargo`、`rustc`、`doggo`），先直接调用命令；只有命令未找到、shim 不可用或未解析到项目工具链等无法直接执行时，才使用 `mise exec --` 桥接，并记录具体回退原因。例如：

```powershell
cargo fmt --manifest-path backend/Cargo.toml -- --check
cargo test --manifest-path backend/Cargo.toml
```

如直调失败且确认需要桥接，再使用对应的 `mise exec -- <command>` 形式，例如 `mise exec -- cargo test --manifest-path backend/Cargo.toml`。

“无法直接执行”不包括 `cargo`、`doggo` 等命令自身返回的编译、测试或请求错误；这类错误应先按命令输出诊断。

执行 DoH 测试前先检查工具是否已存在（例如 `Get-Command doggo`、`Get-Command curl.exe` 或对应系统的 `where` 命令）。

- DoH 测试可以使用已有的 `doggo`、`curl` 及其他系统常见工具；具体参数以本机工具的 `--help` 和版本为准，不在本规范中假定未验证的参数。
- 禁止未经批准安装额外工具。
- 项目当前确实需要且本机缺失的工具（例如检查 SQLite 数据库所需的 SQLite CLI）可以自行安装，但必须确认当前 `mise` 支持管理该工具，并通过 `mise` 安装或切换版本；是否已写入 `mise.toml` 不改变这条安装边界。
- `mise` 不支持的工具、与当前项目任务无关的工具以及其他依赖工具，安装前必须获得明确批准。未经批准不得通过 `cargo install`、`npm install -g`、`pip install`、Scoop、WinGet、Chocolatey 或其他包管理器绕过该规则。
- 如果工具版本需要成为项目共享基线，应同步更新 `mise.toml`；个人临时工具不写入仓库配置。
- 测试脚本应记录实际使用的工具及版本，避免把个人环境中的隐式依赖当成项目要求。

## 4. DoH 本地 smoke test

DoH smoke test 只验证本地启动实例和本次改动涉及的行为，不以远程服务可用性代替本地验证。建议流程如下：

1. 使用 `_fluxdns/config.yaml` 启动 `validate` 或 `run`；端口、证书和资源路径使用本地测试值，避免占用生产端口或写入仓库外的共享数据。
2. 确认 DoH listener 已成功绑定，再使用已有的 `doggo` 或 `curl` 发送 DoH GET/POST 请求。工具缺失时停止该项测试并报告，不临时安装替代工具。
3. 对每个请求记录请求方式、URL 路径、HTTP 状态、`Content-Type`、DNS 响应 ID/RCODE 以及失败原因；必要的查询数据放在 `_fluxdns/` 下的临时文件中。
4. 验证结束后停止服务，检查日志和数据库是否写入预期位置，并确认 `git status --short` 没有出现未忽略的运行时文件。

DoH 测试至少覆盖当前实现支持的 GET 和 POST 入口；HTTP 层错误与 DNS 层错误要分别记录，不能只看到 HTTP 2xx 就判定 DNS 响应正确。测试输出不得包含 SecretRef 实际值、完整认证信息或不必要的原始 DNS wire；共享日志中也不要写入敏感查询参数。

### external 模式边界

- 不得为了测试 DoH `external` 模式而安装 Nginx、Caddy、Traefik 或其他反向代理工具；反向代理不属于本地 DoH 测试的默认依赖。
- 如果现有的 `doggo`、`curl` 或其他已获准工具无法完成 `external` 模式验证，可以跳过该模式，不得为了补齐测试而自行安装反向代理。
- 跳过 `external` 模式时，必须在测试结果中记录未执行的模式、实际尝试过的工具、具体限制和后续需要的环境条件；不得将跳过描述为通过。

## 5. 结果记录

每次本地验证至少记录以下信息：

- 配置文件路径及 `work.path` 的解析结果；
- 执行的命令、工具版本和 Rust toolchain 来源；
- 静态检查、单元测试和 DoH smoke test 的通过/失败结果；
- 端口、临时数据目录和日志位置；
- 未执行项目、失败原因及是否需要额外授权。

文档、代码或配置契约发生变化时，应同步更新本规范中的命令、路径和验证范围。
