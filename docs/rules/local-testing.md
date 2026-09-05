# 本地测试规范

> 文档状态：有效
>
> 适用范围：FluxDNS 本地测试配置、运行时文件和 DoH smoke test 管理

本文档约定 FluxDNS 在本地开发和验证时的测试配置、运行时文件和 DoH smoke test 方式。项目工具链、命令调用、构建物和缓存目录遵循[项目环境使用规范](environment-usage.md)。除非另有说明，命令均从仓库根目录执行。

## 1. 本地目录约定

仓库根目录下的 `_fluxdns/` 是本地测试专用的工作目录。配置文件、规则文件、数据库、测试缓存、日志、临时证书和其他运行时生成文件都放在该目录或其子目录中，不要把这些文件散落在仓库根目录或代码目录中。

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

因此，配置文件为 `_fluxdns/config.yaml` 且 `work.path: ./` 时，`resolved_work_path` 是 `_fluxdns`；`database.path: ./data/fluxdns.sqlite3` 的实际路径是 `_fluxdns/data/fluxdns.sqlite3`，而不是进程当前目录下的 `data/fluxdns.sqlite3`。完整字段规则见[配置参考](../implementation/configuration.md)。

`_fluxdns/config.yaml` 只是本地测试示例，不是启动脚本默认路径。测试内嵌 binary 时必须显式传入配置，启动/status/stop、状态码和 PID 身份保护的完整说明见[交付实现](../implementation/delivery.md)。

测试配置可按场景修改 listener、端口和资源路径，但运行时文件继续限制在 `_fluxdns/`，避免生产端口。状态、stdout/stderr 与临时报告也留在该目录，不提交本地配置或敏感值。

`_fluxdns/` 已加入 `.gitignore`，其中内容默认不应被提交。需要长期维护、可复现并纳入版本控制的测试夹具，应放在专门的受跟踪目录中，不要依赖个人 `_fluxdns/` 内容。

## 2. Git 管理

构建物、依赖目录和工具链缓存的路径、`.gitignore` 约定及新增目录的同步要求见[项目环境使用规范](environment-usage.md)。本地测试配置和运行时数据统一放在 `_fluxdns/`，该目录已加入 `.gitignore`，不得通过 `git add -f` 强行提交本地产物。提交前检查 `git status --short`，确认没有配置、数据库、日志或构建目录进入暂存区。

## 3. DoH 本地 smoke test

DoH smoke test 只验证本地启动实例和本次改动涉及的行为，不以远程服务可用性代替本地验证。建议流程如下：

1. 使用 `_fluxdns/config.yaml` 启动 `validate` 或 `run`；端口、证书和资源路径使用本地测试值，避免占用生产端口或写入仓库外的共享数据。
2. 确认 DoH listener 已成功绑定，再使用已有的 `doggo` 或 `curl` 发送 DoH GET/POST 请求。执行前按[项目环境使用规范](environment-usage.md)确认工具来源；工具缺失时停止该项测试并报告，不临时安装替代工具。
3. 对每个请求记录请求方式、URL 路径、HTTP 状态、`Content-Type`、DNS 响应 ID/RCODE 以及失败原因；必要的查询数据放在 `_fluxdns/` 下的临时文件中。
4. 验证结束后停止服务，检查日志和数据库是否写入预期位置，并确认 `git status --short` 没有出现未忽略的运行时文件。

DoH 测试至少覆盖当前实现支持的 GET 和 POST 入口；HTTP 层错误与 DNS 层错误要分别记录，不能只看到 HTTP 2xx 就判定 DNS 响应正确。测试输出不得包含 SecretRef 实际值、完整认证信息或不必要的原始 DNS wire；共享日志中也不要写入敏感查询参数。

### external 模式边界

- 不得为了测试 DoH `external` 模式而安装 Nginx、Caddy、Traefik 或其他反向代理工具；反向代理不属于本地 DoH 测试的默认依赖。
- 如果现有的 `doggo`、`curl` 或其他已获准工具无法完成 `external` 模式验证，可以跳过该模式，不得为了补齐测试而自行安装反向代理。
- 跳过 `external` 模式时，必须在测试结果中记录未执行的模式、实际尝试过的工具、具体限制和后续需要的环境条件；不得将跳过描述为通过。

## 4. 结果记录

每次本地验证至少记录以下信息：

- 配置文件路径及 `work.path` 的解析结果；
- 执行的测试命令、实际工具版本及 toolchain 来源（环境工具链按[项目环境使用规范](environment-usage.md)核对）；
- 静态检查、单元测试和 DoH smoke test 的通过/失败结果；
- 端口、临时数据目录和日志位置；
- 未执行项目、失败原因及是否需要额外授权。

文档、代码或配置契约发生变化时，应同步更新本规范中的命令、路径和验证范围。
