# FluxDNS

面向策略分流的 DNS 服务。

FluxDNS 计划为 DNS 请求提供基于域名、客户端和规则集的解析决策，并支持常见的 DNS 传输方式；主动上游健康检查属于后续版本能力。

## 项目状态

后端总体架构和 12 个模块方案已经完成，阶段 1“项目骨架与核心契约”和阶段 2“配置系统”已经实现。阶段 3 已完成 Runtime snapshot/preflight、监听绑定全成/全退、原子激活、Supervisor 基础、UDP/TCP 不透明 socket capability、Application CLI/校验接线和基础服务编排；阶段 4 已完成共享 DNS wire codec、固定 Core、UDP/TCP framing、UDP 截断和 TCP 持久 session 小阶段；阶段 5 已完成内联 hosts exchange、hosts registry、group member selection 和 outcome/fallback 判定小阶段；阶段 6 已完成内存 CacheStore、容量淘汰、响应准入/TTL、key builder 和 CacheFacade 首轮切片；阶段 7 已完成 Resource hosts/rule parser、const/file loader、snapshot/CAS、Policy resource matcher 和 DNS Core hosts 接线；阶段 8 已完成 DoH plain HTTP GET/POST、路由匹配和 direct HTTP service 接线；阶段 9 已完成 Storage 统计 epoch/ledger 与 Observability metrics/health registry 纯领域切片。Config 已完成 strict DTO/YAML bounded loader、v1 空迁移 registry、路径和 SecretRef source normalization、semantic validation、reference graph、bind plan、安全快照与不可变 `ResolvedConfig`。当前后端代码实现进度为 46.9%，包含设计阶段的 v1 交付总进度为 52.2%。

当前 `run` 已能通过真实系统 socket 启动 UDP/TCP/DoH plain HTTP 服务，支持内联和 Resource hosts 响应、同一 TCP 连接连续 frame、DoH GET/POST 和 Ctrl-C 优雅停机。DoH 首轮只接受 `tls.mode: external` 与 `client_ip.source: peer`，TLS terminate、forwarded header、PROXY protocol 会在 service 装配阶段明确拒绝，不会误接到 raw DNS/TCP。upstream 当前具备内联 hosts connector、纯选择器和 outcome/fallback 判定，但尚未执行真实出站 I/O 或接入 DNS Core。cache 当前具备内存 adapter 的 lookup、质量 CAS、失效、single-flight、响应准入/TTL、key builder、CacheFacade 和共享容量淘汰，optimistic refresh 与持久化仍未完成；policy 当前具备 client ID/CIDR、strategy、listener/DoH route、const/file resource loader 和 rule/hosts 请求级 plan 首轮组合；resource 当前具备 hosts/rule parser、受限 regex、const/file loader 和 snapshot/CAS；storage 当前具备内存 stats epoch/ledger；observability 当前具备有界 metrics/health registry。Runtime snapshot 资源原子接线、remote refresh、DoH outbound/bootstrap、Moka/SQLite persistence、详情/final writer 和完整 DNS Core→Policy→Cache→Upstream 管线仍未完成。SecretRef 实际值不会由普通 YAML load 读取，仅由后续 adapter 通过显式 accessor 请求。

## 仓库布局

- `backend/`：Rust 后端的独立主目录，包含 `Cargo.toml`、`Cargo.lock` 与 `src/`；
- `frontend/`：前端的独立主目录，当前尚未初始化具体技术栈；
- `docs/`：仓库级技术文档，入口见 [docs/README.md](docs/README.md)；
- `config-example.yaml`：仓库级配置示例；
- `_fluxdns/`：本地测试配置、运行数据和日志目录，仅供本机使用，不提交到 Git。

仓库根目录不作为前端或后端的代码主目录。后端验证命令从根目录执行时使用 `--manifest-path backend/Cargo.toml`。

## 目标

- 提供传统 DNS 和 DNS over HTTPS（DoH）服务；DNS over TLS（DoT）和 DNS over QUIC（DoQ）是后续版本目标。
- 根据域名、客户端与规则集，将请求分流到合适的解析策略和上游。
- 支持本地与远程规则集、缓存、上游组与故障回退。
- 保持配置清晰，并在策略或上游不可用时提供可诊断的失败信息。

## 配置

[config-example.yaml](config-example.yaml) 是配置草案。本地运行时应复制为 `_fluxdns/config.yaml` 后按测试环境调整；配置、规则文件、运行数据、缓存与日志均放在 `_fluxdns/` 下，不提交到 Git。

完整文档入口见 [docs/README.md](docs/README.md)。字段语义见 [docs/configuration-reference.md](docs/configuration-reference.md)，本地测试约定见 [docs/standards/local-testing.md](docs/standards/local-testing.md)，项目协作规则见 [AGENTS.md](AGENTS.md)，Rust 后端总体方案见 [docs/backend-architecture.md](docs/backend-architecture.md)，模块方案、阶段安排和当前进度见 [docs/backend-development-plan.md](docs/backend-development-plan.md)。

Config 阶段 2 记录起点为 69 tests；上次后端全量验证为 238 passed、0 failed，`clippy --all-targets -- -D warnings` 和 `fmt --check` 均已通过。本次新增配置路径解析测试后，需在可用 Rust 1.98 toolchain 下重新复验最新结果。真实 smoke 使用临时配置在 UDP `127.0.0.1:8353`、TCP `127.0.0.1:8354` 和 DoH `127.0.0.1:8355` 验证 hosts 响应、同连接双 frame、DoH GET/POST 的 DNS ID/RCODE 和 `SIGINT` 停机；端口仅用于本机验证，不改变配置契约。阶段 5 的 hosts/group/outcome 定向测试覆盖格式解析、DNS outcome、取消/超时、registry fail-closed、选择器并发 lease、terminal/fallback 判定和 connector 去重；阶段 6 的内存 cache 定向测试覆盖 fresh/stale/expiry、质量 CAS、显式失效、single-flight cancellation/abandon、shutdown、响应分类、TTL、stale 窗口、checksum、容量淘汰，key/facade 定向测试覆盖稳定编码和 lookup/write 状态；阶段 7 的 Resource/DNS/Policy 定向测试覆盖 hosts/rule parser、受限 regex、const/file loader、snapshot/CAS、CNAME/wildcard、listener hosts 优先、strategy rule 顺序、缺失资源和 file 接线。阶段 9 的 Storage/Observability 定向测试覆盖 UTC day、epoch swap、幂等 ledger、persistence gap、有界 metrics、health recovery、retry/gap 和 typed event 脱敏。阶段 2 的配置示例校验不访问远程资源、不执行资源首次 snapshot。

配置中的示例地址、账号、密码和客户端标识仅用于说明格式，部署前必须替换为实际且受保护的配置。

## 命名约定

- 项目与仓库目录：`FluxDNS`
- 二进制与服务标识：`fluxdns`
- 项目描述：`A policy-driven DNS server`
