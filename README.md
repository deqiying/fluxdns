# FluxDNS

面向策略分流的 DNS 服务。

FluxDNS 计划为 DNS 请求提供基于域名、客户端和规则集的解析决策，并支持常见的 DNS 传输方式；主动上游健康检查属于后续版本能力。

## 项目状态

后端总体架构和 12 个模块方案已经完成，阶段 1“项目骨架与核心契约”和阶段 2“配置系统”已经实现。阶段 3 已完成 Runtime snapshot/preflight、监听绑定全成/全退、原子激活、Supervisor 基础、UDP/TCP 不透明 socket capability、Application CLI/校验接线和基础服务编排；阶段 4 已完成共享 DNS wire codec、固定 Core、UDP/TCP framing、UDP 截断和 TCP 持久 session 小阶段；阶段 5 已完成内联 hosts exchange、hosts registry 和 group member selection 小阶段；阶段 6 已完成内存 CacheStore、响应准入/TTL、key builder 和 CacheFacade 首轮切片；阶段 7 已完成 Policy client/strategy immutable index 首轮切片；阶段 8 已完成 DoH plain HTTP GET/POST、路由匹配和 direct HTTP service 接线。Config 已完成 strict DTO/YAML bounded loader、v1 空迁移 registry、路径和 SecretRef source normalization、semantic validation、reference graph、bind plan、安全快照与不可变 `ResolvedConfig`。当前后端代码实现进度为 36.7%，包含设计阶段的 v1 交付总进度为 43.0%。

当前 `run` 已能通过真实系统 socket 启动 UDP/TCP/DoH plain HTTP 服务，支持内联 hosts 响应、同一 TCP 连接连续 frame、DoH GET/POST 和 Ctrl-C 优雅停机。DoH 首轮只接受 `tls.mode: external` 与 `client_ip.source: peer`，TLS terminate、forwarded header、PROXY protocol 会在 service 装配阶段明确拒绝，不会误接到 raw DNS/TCP。upstream 当前已具备内联 hosts connector 和纯选择器，但尚未接入 DNS Core、DoH 出站、bootstrap 或 fallback 执行。cache 当前已具备内存 adapter 的 lookup、质量 CAS、失效、single-flight、响应准入/TTL、key builder 和 CacheFacade 首轮能力，容量淘汰和持久化仍未完成；policy 当前已具备 client ID/CIDR 与 strategy lookup 索引，但 rule/route/override 尚未接线；resource、storage 仍未完成。SecretRef 实际值不会由普通 YAML load 读取，仅由后续 adapter 通过显式 accessor 请求。

## 仓库布局

- `backend/`：Rust 后端的独立主目录，包含 `Cargo.toml`、`Cargo.lock` 与 `src/`；
- `frontend/`：前端的独立主目录，当前尚未初始化具体技术栈；
- `docs/`：仓库级架构、模块方案和开发计划；
- `config-example.yaml`：仓库级配置示例。

仓库根目录不作为前端或后端的代码主目录。后端验证命令从根目录执行时使用 `--manifest-path backend/Cargo.toml`。

## 目标

- 提供传统 DNS 和 DNS over HTTPS（DoH）服务；DNS over TLS（DoT）和 DNS over QUIC（DoQ）是后续版本目标。
- 根据域名、客户端与规则集，将请求分流到合适的解析策略和上游。
- 支持本地与远程规则集、缓存、上游组与故障回退。
- 保持配置清晰，并在策略或上游不可用时提供可诊断的失败信息。

## 配置

[config-example.yaml](config-example.yaml) 是配置草案。实际运行时可复制为 `config.yaml` 后按部署环境调整；`config.yaml`、规则文件、运行数据、缓存与日志均不会被 Git 跟踪。

字段语义见 [docs/configuration-reference.md](docs/configuration-reference.md)，Rust 后端总体方案见 [docs/backend-architecture.md](docs/backend-architecture.md)，模块方案、阶段安排和当前进度见 [docs/backend-development-plan.md](docs/backend-development-plan.md)。

Config 阶段 2 记录起点为 69 tests；当前后端全量测试为 184 passed、0 failed，`clippy --all-targets -- -D warnings` 和 `fmt --check` 均已通过。真实 smoke 使用临时配置在 UDP `127.0.0.1:8353`、TCP `127.0.0.1:8354` 和 DoH `127.0.0.1:8355` 验证 hosts 响应、同连接双 frame、DoH GET/POST 的 DNS ID/RCODE 和 `SIGINT` 停机；端口仅用于本机验证，不改变配置契约。阶段 5 的 hosts/group 定向测试覆盖格式解析、DNS outcome、取消/超时、registry fail-closed 和选择器并发 lease；阶段 6 的内存 cache 定向测试覆盖 fresh/stale/expiry、质量 CAS、显式失效、single-flight cancellation/abandon、shutdown、响应分类、TTL、stale 窗口和 checksum；阶段 7 的 Policy 定向测试覆盖 exact ID 优先、IPv4/IPv6 longest-prefix、unknown、重复 matcher 拒绝和 strategy lookup。阶段 2 的配置示例校验不访问远程资源、不执行资源首次 snapshot。

配置中的示例地址、账号、密码和客户端标识仅用于说明格式，部署前必须替换为实际且受保护的配置。

## 命名约定

- 项目与仓库目录：`FluxDNS`
- 二进制与服务标识：`fluxdns`
- 项目描述：`A policy-driven DNS server`
