# FluxDNS

面向策略分流的 DNS 服务。

FluxDNS 计划为 DNS 请求提供基于域名、客户端和规则集的解析决策，并支持常见的 DNS 传输方式；主动上游健康检查属于后续版本能力。

## 项目状态

后端总体架构和 12 个模块方案已经完成，阶段 1“项目骨架与核心契约”和阶段 2“配置系统”已经实现。阶段 3 已完成 Runtime snapshot/preflight、监听绑定全成/全退、原子激活、Supervisor 基础和 UDP/TCP 不透明 socket capability 五个小阶段。Config 已完成 strict DTO/YAML bounded loader、v1 空迁移 registry、路径和 SecretRef source normalization、semantic validation、reference graph、bind plan、安全快照与不可变 `ResolvedConfig`。当前后端实现进度为 17.4%，包含设计阶段的 v1 交付总进度为 25.7%。

当前 App 仍是阶段 1 scaffold；真实 transport framing、upstream/storage adapter、资源网络首次 snapshot、Application 启动接线和 DNS 服务闭环尚未完成。Runtime 已具备真实系统 socket prepare/activate capability，但尚未启动 DNS 请求任务。SecretRef 实际值不会由普通 YAML load 读取，仅由后续 adapter 通过显式 accessor 请求。

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

Config 阶段 2 记录起点为 69 tests；当前工作树已增量至 94 tests，串行测试为 94 passed、0 failed，`clippy` 和 `fmt --check` 均已通过。测试数量可能随后续阶段增量。阶段 2 的配置示例校验不访问远程资源、不执行资源首次 snapshot。

配置中的示例地址、账号、密码和客户端标识仅用于说明格式，部署前必须替换为实际且受保护的配置。

## 命名约定

- 项目与仓库目录：`FluxDNS`
- 二进制与服务标识：`fluxdns`
- 项目描述：`A policy-driven DNS server`
