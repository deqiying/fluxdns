# Docker 镜像交付方案

> 文档状态：有效
>
> 适用范围：FluxDNS 容器镜像的独立构建流水线、运行契约、部署示例与验收方式
>
> 计划状态：待验收
>
> 代码基线：`e3e21c8`

## 1. 问题与现状依据

FluxDNS 当前只有四平台压缩归档这一种交付形态（见[交付实现](../implementation/delivery.md)与 [release.yml](../../.github/workflows/release.yml)），没有容器镜像。容器化不能照搬通用 Docker 惯例，因为本项目的启动、配置路径、特权端口和客户端识别都受明确契约约束。本轮只读核对的结论如下。

| 契约 | 事实 | 依据 |
| --- | --- | --- |
| 进程入口 | `fluxdns [run\|validate] [--config PATH]`；`--version` 输出 `fluxdns <版本>`；不传 `--config` 时默认读取进程当前目录下的 `config.yaml` | [app.rs](../../backend/src/app.rs) |
| 配置快照 | 源配置文件不是 `<work.path>/config.yaml` 时，启动会生成该固定路径的副本；目标已存在且内容不同时拒绝启动 | [配置参考](../implementation/configuration.md) |
| 运行期写入 | 配置、日志、SQLite、详情分片、缓存快照、规则文件都落在 `work.path`；`validate` 使用 `without_snapshot()`，不写盘也不校验 SecretRef | app.rs、[配置参考](../implementation/configuration.md) |
| 编译期需要 migrations | `storage/sqlite.rs` 与 `storage/detail_shards.rs` 在生产路径用 `include_str!` 引用 `backend/migrations/`，构建必须包含该目录，运行期不需要 | [sqlite.rs](../../backend/src/storage/sqlite.rs)、[detail_shards.rs](../../backend/src/storage/detail_shards.rs) |
| 编译期不需要模板与测试夹具 | `config-example.yaml`、`backend/tests/` 只在 `cfg(test)` 下被引用，release 构建用不到；SPA 由 `webui-embed` 在编译期内嵌 | app.rs、[Cargo.toml](../../backend/Cargo.toml) |
| 停机 | SIGINT/SIGTERM 触发 5s 有界优雅停机 | [service.rs](../../backend/src/service.rs)、app.rs |
| 监听端口 | 端口只来自配置文件（示例为 53、443、8053、8080），没有 CLI 或环境变量覆盖入口 | [config-example.yaml](../../config-example.yaml) |
| 客户端地址 | `endpoint.client_ip.source: peer` 取 TCP 对端地址 | config-example.yaml |
| SecretRef | 支持 `env` 与 `file` 两种来源；`run` 启动即校验，缺失直接失败 | app.rs、config-example.yaml |
| 发布产物命名 | 归档为 `fluxdns_<版本>_linux_x86_64.tar.gz`，归档内程序名为 `fluxdns` | release.yml |
| 事件触发限制 | `GITHUB_TOKEN` 引发的事件不会启动新的 workflow run（例外只有 `workflow_dispatch` 与 `repository_dispatch`） | GitHub 行为；`release: published` 不能作为独立 workflow 的触发器，因此使用人工推 tag 的 `push: tags` |
| 许可证 | 仓库根目录没有 `LICENSE` | 仓库根目录 |

## 2. 目标与非目标

目标：

1. 镜像只由 GitHub Actions 构建，推送到 GitHub Container Registry（GHCR）；本地不构建镜像，也不要求本机安装容器工具链。
2. 镜像构建是一条**完全独立的 workflow**：不写入 `release.yml`，不等待 Release workflow 成功，也不依赖其归档或 `checksums.txt`；由同一个 tag 推送事件自行触发、自行编译、自行发布。
3. 运行契约明确可复现：工作目录、配置文件位置、挂载点、端口、运行身份、停机与数据落盘。
4. 提供可直接复制的 `docker/compose-example.yaml` 与 `docker/.env.example`，覆盖端口映射、卷、SecretRef 与重启策略。
5. 镜像级验证可在 CI 内自动执行，不依赖人工浏览器或真实设备。

非目标：

1. 不新增 `linux/arm64`、musl/静态或其他平台变体。
2. 不修改既有四平台发布矩阵、归档命名、版本门禁与 `checksums.txt` 语义，也不修改 `release.yml`。
3. 不在本地构建镜像；不引入 GHCR 之外的镜像仓库、Kubernetes/Helm 清单或镜像签名工具。
4. 不修改 DNS、Management 的运行时行为，不新增配置字段。
5. 不在镜像流水线内重跑 Clippy 与完整测试套件（属发布流程职责，取舍见 6.1）。

## 3. 已确认决策

| 决策 | 选择 | 影响 |
| --- | --- | --- |
| 流水线形态 | 独立 workflow 文件，不追加到 `release.yml` | 与发布流程解耦，各自维护触发与门禁 |
| 独立程度 | 完整独立：不等待 Release 成功，不使用其归档 | 镜像内二进制由本 workflow 自行编译；与发布归档不是同一份字节（已接受，见 6.1） |
| 编译位置 | 在 runner 上编译二进制，Dockerfile 只做 COPY | 复用与 `release.yml` 相同的构建步骤和 `actions/cache` 增量缓存；镜像内不含工具链 |
| 工作目录 | 容器内 `work.path` 固定为 `/etc/fluxdns`，配置文件固定为 `/etc/fluxdns/config.yaml` | 与根 `config-example.yaml` 的 `work.path` 示例取值一致，运行时无需二次修改 |
| 运行身份 | 镜像默认以 root（uid 0）运行 | 可直接绑定 53/443，不依赖 file capability，因此可叠加 `no-new-privileges` |
| 网络模式 | 默认 bridge + 端口映射 | 跨平台可用；代价是 `peer` 客户端地址恒为 docker 网关地址，需在文档中记录该限制 |
| 架构 | 仅 `linux/amd64` | 与发布矩阵的 x86_64 一致 |
| 部署示例 | 提供 `docker/compose-example.yaml` 与 `docker/.env.example` | 部署者复制后即可运行，不把真实凭据写进仓库 |
| 本轮范围 | 只新增本方案与计划索引项 | Dockerfile、workflow、部署示例与长期文档改动在实施批次完成 |

## 4. 变更设计

### 4.1 文件改动清单

| 文件 | 动作 | 内容 |
| --- | --- | --- |
| `.github/workflows/container.yml` | 新增 | 独立的编译、镜像构建与推送 workflow |
| `docker/Dockerfile` | 新增 | 运行镜像定义（只做 COPY，不含编译） |
| `.dockerignore` | 新增 | 缩小构建上下文：排除构建产物、依赖目录、本地运行目录与归档 |
| `docker/compose-example.yaml` | 新增 | 部署示例（端口映射、卷、SecretRef、重启策略、健康检查） |
| `docker/.env.example` | 新增 | 部署示例的环境变量模板（镜像引用、宿主路径、端口、SecretRef） |
| `docker/container-smoke-config.yaml` | 新增 | 镜像冒烟夹具：绑定 loopback 53、`logs.enable: true`、只用内联 hosts |
| `.gitignore` | 修改 | 忽略部署者实际的 `docker/.env`，避免凭据入库 |
| [交付实现](../implementation/delivery.md) | 修改 | 镜像构建、发布、运行契约与限制的唯一权威说明 |
| [环境规则](../rules/environment-usage.md) | 修改 | 明确镜像只在 Actions 构建，本地不要求容器工具链 |
| [README](../../README.md) | 修改 | 仓库布局与最短使用入口 |
| `docs/plans/docker-image.md` | 删除 | 实施与验收完成后按维护规则移除 |

`release.yml` 不在改动清单内：镜像 workflow 是新增文件，两个 workflow 互不引用。

### 4.2 运行镜像

镜像内容只有二进制、CA 证书与最小运行库；不装编译工具链、不装 tini、不复制 `backend/migrations` 或 `frontend/dist`（两者在编译期已内嵌）。

```dockerfile
# syntax=docker/dockerfile:1
FROM debian:trixie-slim

# 远程规则集与 DoH 上游需要可信 CA。
RUN apt-get update \
 && apt-get install --yes --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

ARG FLUXDNS_VERSION=0.0.0
ARG FLUXDNS_REVISION=unknown
LABEL org.opencontainers.image.title="FluxDNS" \
      org.opencontainers.image.description="A policy-driven DNS server" \
      org.opencontainers.image.source="https://github.com/deqiying/fluxdns" \
      org.opencontainers.image.version="${FLUXDNS_VERSION}" \
      org.opencontainers.image.revision="${FLUXDNS_REVISION}"

# 二进制由 workflow 在本 runner 编译后暂存到 deploy/；镜像内不含工具链，也不编译。
COPY deploy/fluxdns /usr/local/bin/fluxdns

# 工作目录固定为 /etc/fluxdns，配置快照路径与之一致，部署时整目录挂载。
RUN install -d -o 0 -g 0 -m 0750 /etc/fluxdns

# 默认以 root 运行以便绑定 53/443；权限收紧由部署层负责。
WORKDIR /etc/fluxdns
EXPOSE 53/udp 53/tcp 443/tcp 8080/tcp
ENTRYPOINT ["/usr/local/bin/fluxdns"]
CMD ["run", "--config", "/etc/fluxdns/config.yaml"]
```

设计说明：

- **不在镜像内编译**：`webui-embed` 需要 `frontend/dist`、SQLite 迁移需要 `backend/migrations/`，在 runner 上编译可直接复用既有构建步骤与缓存，Dockerfile 保持只有 COPY。
- **build 上下文使用 `deploy/` 暂存二进制**：该目录已 gitignore，是仓库既有的发布物暂存约定，避免 `backend/target` 进上下文的排除/白名单冲突。
- **不写 `VOLUME`**：避免 `docker run` 隐式创建匿名卷，把卷决策交给 4.5 的部署示例显式声明。
- **不装 init 进程**：Rust 进程自行处理 SIGTERM 且不派生需回收的子进程，Docker 默认 10s stop timeout 已覆盖 5s 宽限期。
- **不引入 shell 工具**：镜像内不提供 `curl`、`dig` 等客户端，DNS 级探针列入人工验收而不是镜像依赖。
- **工作目录与配置路径一致**：`WORKDIR`、`CMD` 与配置里的 `work.path` 都是 `/etc/fluxdns`，避免生成第二份配置快照。
- 基础镜像必须满足编译产物的 glibc 下限；`debian:trixie-slim` 满足（见 6.1）。
- 可选 `HEALTHCHECK`：以 `fluxdns validate --config /etc/fluxdns/config.yaml` 作为配置级探针。它只证明配置可解析与路径合法，不证明 listener 正在服务，也不校验 SecretRef；若担心语义误读，可以不定义，由 4.5 的 Compose 健康检查或外部探针承担。

### 4.3 挂载与配置契约

| 项 | 决定 | 原因 |
| --- | --- | --- |
| `work.path` | `/etc/fluxdns` | 与根模板示例一致，规则、数据、日志、TLS 的相对路径都落在这里 |
| 配置文件位置 | 必须是 `/etc/fluxdns/config.yaml` | 否则启动会生成第二份快照，且目标内容不同时拒绝启动 |
| 挂载方式 | 整目录挂载宿主机目录到 `/etc/fluxdns`，不要单独 bind mount `config.yaml` | Management 的配置 apply/还原用同目录原子替换；整文件 bind mount 上的替换会 `EBUSY` |
| 首次准备 | 宿主目录里先放好 `config.yaml`（可从 `config-example.yaml` 复制），需要时再放 `tls/`、`rules/` | 该目录同时是配置、数据与规则根 |
| 容器命令 | 保持 `CMD ["run", "--config", "/etc/fluxdns/config.yaml"]` | 与工作目录和快照路径一致 |
| WebUI 可见性 | 需要从容器外访问时 `webui.address: 0.0.0.0`，且 `public_origin` 必须是浏览器实际访问的 origin | 示例中 `webui.address` 为 `127.0.0.1`，容器外不可达 |
| TLS | 默认落在 `/etc/fluxdns/tls/`（由卷覆盖）；也可改绝对路径并单独只读挂载 | DoH `endpoint.tls.mode: terminate` 需要证书链与私钥；证书轮换按重启容器处理（运行期是否重读证书文件本轮未核对） |
| SecretRef | 通过环境变量或只读文件注入，不写进镜像 | `run` 启动即校验，缺失会让容器启动失败 |
| 目录归属 | 宿主目录归 root 所有（与 root 运行一致） | 若部署层加 `--cap-drop ALL`，root 会失去 `CAP_DAC_OVERRIDE`，目录归属必须匹配 |

不为容器另建第二份配置模板：根 `config-example.yaml` 仍是唯一模板，容器差异以上表字段为准。

### 4.4 网络、端口与客户端地址

默认 bridge + 端口映射的发布方式：

| 用途 | 容器内 | 宿主映射 |
| --- | --- | --- |
| DNS UDP/TCP | 53 | `53:53/udp`、`53:53/tcp` |
| DoH terminate | 443 | `443:443/tcp` |
| Management/WebUI | 8080（`webui.address: 0.0.0.0`） | `127.0.0.1:8080:8080`，仅本机可达 |

已接受的限制：bridge 模式下 DNS 客户端的 `peer` 地址是 docker 网关地址，因此 `clients[].match.ips` 无法区分真实局域网客户端，ECS `mode: client` 的前缀推导也会失真。可选缓解方式：

1. DoH 使用 `endpoint.client_ip.source: forwarded_header` 或 `proxy_protocol`，由容器前的反向代理提供真实地址，并在 `trusted_proxies` 中写反代/docker 网关地址。
2. 改为 `network_mode: host`（Linux），恢复真实客户端地址；这不需要改动镜像，只改部署方式。

### 4.5 部署示例

`docker/compose-example.yaml`（部署者复制为 `docker/compose.yaml` 使用）：

```yaml
name: fluxdns

services:
  fluxdns:
    image: ${FLUXDNS_IMAGE}
    container_name: fluxdns
    restart: unless-stopped
    # SecretRef 的实际值通过 env_file 注入容器环境，不写进镜像与仓库。
    env_file:
      - .env
    ports:
      # DNS 同时需要 UDP 与 TCP。
      - "${FLUXDNS_DNS_PORT:-53}:53/udp"
      - "${FLUXDNS_DNS_PORT:-53}:53/tcp"
      - "${FLUXDNS_DOH_PORT:-443}:443/tcp"
      # Management/WebUI 默认只监听宿主机回环。
      - "${FLUXDNS_WEBUI_BIND:-127.0.0.1}:${FLUXDNS_WEBUI_PORT:-8080}:8080"
    volumes:
      # 整目录挂载：配置、日志、数据库、规则与 TLS 都在这里。
      - ${FLUXDNS_WORK_DIR:-/srv/fluxdns}:/etc/fluxdns
    # root 运行下仍收紧权限：只保留绑定特权端口所需能力。
    cap_drop:
      - ALL
    cap_add:
      - NET_BIND_SERVICE
    security_opt:
      - no-new-privileges:true
    stop_grace_period: 20s
    # 镜像内没有 shell 工具，使用二进制自身做配置级探针。
    healthcheck:
      test: ["CMD", "/usr/local/bin/fluxdns", "validate", "--config", "/etc/fluxdns/config.yaml"]
      interval: 30s
      timeout: 10s
      retries: 3
      start_period: 10s
    logging:
      driver: json-file
      options:
        max-size: "10m"
        max-file: "3"
```

`docker/.env.example`（部署者复制为 `docker/.env`，该文件已在 `.gitignore` 中）：

```dotenv
# 镜像引用：示例跟随 latest，便于复制后直接拉取；需要可复现部署时替换为具体版本 tag。
FLUXDNS_IMAGE=ghcr.io/deqiying/fluxdns:latest

# 宿主工作目录：必须预先存在并包含 config.yaml（work.path 为 /etc/fluxdns）。
FLUXDNS_WORK_DIR=/srv/fluxdns

# 端口映射：53/443 需要宿主端口未被占用。
FLUXDNS_DNS_PORT=53
FLUXDNS_DOH_PORT=443
FLUXDNS_WEBUI_BIND=127.0.0.1
FLUXDNS_WEBUI_PORT=8080

# SecretRef 示例：与配置中的 outbound.proxy_url.env 名称一致。
# 含凭据的文件不得提交；也可改用 Compose secrets 并在配置里使用 file 来源。
# FLUXDNS_OUTBOUND_SG_URL=socks5://user:password@proxy.example:1080
```

示例说明：

- **同一份 `.env` 承担两种作用**：Compose 变量替换（镜像引用、宿主路径、端口）与容器环境变量注入（SecretRef 实际值）。含凭据时不得提交。
- **示例默认引用 `latest`**：复制后即可拉取。需要可复现或可回滚的部署时改成具体版本 tag（如 `:0.3.5`）；workflow 同时推送 `<版本>`、`<主>.<次>` 与 `latest`，其中 `latest` 只在非预发布 tag 上移动。
- **需要写入数据的部署必须先把宿主目录准备好**：目录缺失或缺少 `config.yaml` 时容器会启动失败，不会自动生成生产配置。
- **也可用文件式凭据**：Compose `secrets` 挂到 `/run/secrets/...`，配置里改用 `proxy_url.file`，避免凭据出现在容器环境变量中。

### 4.6 独立 CI 流水线

新增 `.github/workflows/container.yml`，由 tag 推送独立触发，自己编译、自己构建、自己推送；不使用 Release workflow 的归档，也不等待其成功。

```yaml
name: Container image

on:
  # 人工推送 tag 触发，与 release.yml 同源但互不依赖。
  push:
    tags:
      - "v*"
  # 手动补发或重跑指定 tag。
  workflow_dispatch:
    inputs:
      tag:
        description: "已存在的 tag，例如 v0.3.5"
        required: true

permissions:
  contents: read

concurrency:
  group: container-${{ github.ref }}
  cancel-in-progress: false

jobs:
  publish:
    name: Build and push image
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      # 1. checkout（fetch-depth 0，tag 归属与版本校验需要历史）
      # 2. 解析 tag：push 用 GITHUB_REF_NAME，手动触发用 inputs.tag
      # 3. 校验 tag：合法 SemVer、等于 VERSION/Cargo/前端 package 版本、提交属于 main
      # 4. 从 mise.toml 读取 rust/node/pnpm 版本，保持与项目工具链同一来源
      # 5. setup-node/setup-pnpm -> pnpm install --frozen-lockfile -> pnpm run build（生成 frontend/dist）
      # 6. setup-rust-toolchain（含 actions/cache 复用 ~/.cargo 与 backend/target）
      #    -> cargo build --manifest-path backend/Cargo.toml --locked --release --features webui-embed
      # 7. 校验 backend/target/release/fluxdns --version 等于 "fluxdns <version>"，并暂存为 deploy/fluxdns
      # 8. setup-buildx -> login ghcr.io（secrets.GITHUB_TOKEN）-> 显式计算镜像标签 -> build-push-action（linux/amd64）
      # 9. 镜像级验证：docker run --rm <image> --version；再用挂载的模板配置执行 validate
```

| 决策点 | 选择 | 理由 |
| --- | --- | --- |
| 触发方式 | `push: tags: v*` 与 `workflow_dispatch` | 人工推 tag 可正常启动独立 workflow；`release: published` 由 `GITHUB_TOKEN` 触发，无法用于此目的 |
| 编译位置 | runner 上的 pnpm + cargo，Dockerfile 只 COPY | 复用 `release.yml` 已验证的构建步骤与缓存键；镜像内不留工具链 |
| 编译输入 | `backend/`（含 `migrations/`）与 `frontend/` | 生产路径 `include_str!` 引用 migrations；`webui-embed` 需要 `frontend/dist` |
| 二进制校验 | `--version` 必须等于 tag 版本后再进镜像 | 阻止镜像内出现与 tag 不符的二进制 |
| 平台 | `linux/amd64` | 与发布矩阵 x86_64 一致 |
| 标签 | `<版本>`、`<主>.<次>`、`latest`（仅非预发布）与 `sha-<短哈希>` | 由 workflow 显式计算：检出 tag 后不依赖 `metadata-action` 对 `GITHUB_REF` 的解释 |
| 版本门禁 | 复制 `release.yml` 的 tag/VERSION/Cargo/前端一致性校验 | 独立流水线不能省掉版本门禁；重复的代价见 6.1 |
| 权限 | `contents: read`、`packages: write` | 检出源码并推送 GHCR |
| 可选增强 | 追加 `id-token: write`、`attestations: write` 输出 provenance/SBOM | 不在首批必需范围内 |
| 包可见性 | 默认私有；公开需在仓库 Packages 设置或另行调用 API | 不由 workflow 自动变更，避免扩大权限 |

备选（未采用）：把前端与后端编译放进 Dockerfile 多阶段构建。它让镜像定义自包含，但 GitHub 托管 runner 是临时的，BuildKit 的缓存挂载无法跨 run 复用，每次都要完整重编译 Rust；同时工具链版本要在 Dockerfile 里再维护一份。

## 5. 实施步骤

1. 新增 `docker/Dockerfile`，实现 4.2 的运行镜像（`COPY deploy/fluxdns`）。
2. 新增 `.dockerignore`，排除 `backend/target`、`frontend/node_modules`、`frontend/dist`、`_fluxdns`、`.git` 与 `deploy/` 中的归档。
3. 新增 `docker/compose-example.yaml`、`docker/.env.example` 与 `docker/container-smoke-config.yaml`，实现 4.5 的部署示例与镜像冒烟夹具；同步在 `.gitignore` 忽略 `docker/.env`。
4. 新增 `.github/workflows/container.yml`，实现 4.6 的版本门禁、前端与后端编译、二进制校验、镜像构建推送与镜像级验证；不修改 `release.yml`。
5. 更新 [交付实现](../implementation/delivery.md)：新增容器镜像章节，说明独立构建入口、发布产物、运行契约（工作目录 `/etc/fluxdns`）、部署示例与已知限制。
6. 更新 [环境规则](../rules/environment-usage.md)：镜像只在 Actions 构建，本地不安装容器工具链，也不产生本地镜像产物。
7. 更新 [README](../../README.md)：仓库布局加 `docker/` 与容器 workflow，补 GHCR 拉取与运行的最短入口。
8. 本批次不执行镜像构建；本地只做静态检查（文档检查器与 `git diff --check`），镜像行为由 tag 推送后的独立 workflow 验证。
9. 首次发布后核对镜像与部署示例的一致性；通过后按维护规则删除本方案与计划索引项。

## 6. 风险与未验证边界

### 6.1 已知风险

| 风险 | 说明 | 处置 |
| --- | --- | --- |
| 重复编译 | 同一个 tag 下 `release.yml` 与容器 workflow 各编译一次 Linux 产物，CI 时间与额度翻倍 | 已接受；这是“完全独立”的必然代价，若改为复用归档则失去独立性 |
| 产物非同一份字节 | 镜像内二进制与 Release 归档、`checksums.txt` 各自编译，字节不保证一致 | 已接受；两者都由同一 tag 源码与本 workflow 的 `--version` 校验约束 |
| 版本逻辑重复 | tag/VERSION/Cargo/前端一致性校验在 `release.yml` 与容器 workflow 各有一份 | 保持两份并同步修改；不为此改动 `release.yml`（可后续抽公共脚本，另行评估） |
| 质量门禁不重叠 | 容器 workflow 默认不跑 Clippy 与测试套件，只做编译与 `--version`/`validate` 检查 | 已接受；由发布流程承担测试职责。若要求镜像自带门禁，需追加 Clippy/测试步骤并计入 CI 时间 |
| glibc 下限 | runner 为 `ubuntu-latest`，若运行镜像 glibc 低于编译产物要求，容器启动即失败 | 使用 `debian:trixie-slim`；更小体积的 distroless/`ubuntu:24.04` 需另行实测 |
| 客户端地址失真 | bridge 模式下 `peer` 恒为 docker 网关地址 | 已接受；按 4.4 的缓解方式或 host 网络处理，并在交付文档中明确 |
| 凭据入库 | 部署示例的 `.env` 需要承载 SecretRef 实际值 | `.gitignore` 忽略 `docker/.env`，示例文件只放占位与注释 |
| 权限收紧与卷归属 | 示例使用 `cap_drop: ALL`，root 会失去 `CAP_DAC_OVERRIDE`，宿主目录必须归 root 所有 | 写入交付文档的运行要求；目录归属不匹配时可调整 `cap_drop` 或目录 owner |
| 缺少许可证 | 仓库无 `LICENSE`，镜像不能声明 licenses label | 保持不声明；是否需要补许可证另行决策 |

### 6.2 未验证项

1. 镜像体积、容器内 DNS/DoH 吞吐与延迟均未测量。
2. 未执行任何真实 Actions 运行、GHCR 推送、`docker compose` 启动或镜像运行；Rust 在 runner 上的完整编译耗时未测量。
3. DoH 证书在运行期是否被重新读取未核对，方案按重启容器处理。
4. 运行根文件系统只读、`tmpfs` 挂载等额外加固组合未验证（生产代码未使用 `std::env::temp_dir`，但仍需实测确认无其他写入路径）。
5. 备选方案（Dockerfile 内多阶段编译）在 GitHub 托管 runner 上的缓存与耗时未评估。
6. `debian:trixie-slim` 之外的基础镜像、musl 静态产物能否直接用于容器均未验证。

## 7. 验证与退出条件

### 7.1 CI 内自动化验证（实施批次落地）

1. 容器 workflow 内编译出的 `fluxdns --version` 等于 tag 版本，且 tag 版本与 `VERSION`/Cargo/前端 package 一致。
2. `docker run --rm <image> --version` 输出等于 `fluxdns <version>`。
3. 用挂载的模板配置（`config-example.yaml` 复制为 `config.yaml`）执行 `validate`，退出码为 0，且挂载目录没有新增 `config.yaml` 快照（验证 `without_snapshot` 行为与工作目录一致性）。
4. 以真实 `run` 启动并绑定 53，日志出现 `service_ready` 且无绑定权限错误。
5. `docker stop` 在 5s 宽限期内完成优雅停机。
6. `docker compose -f docker/compose-example.yaml config` 在提供 `.env` 后解析成功，证明示例语法与变量引用有效。

### 7.2 人工验收（目标主机）

1. 按 4.5 复制示例、准备 `/srv/fluxdns/config.yaml` 后启动，容器进入 healthy。
2. 局域网内用 `dig`/`doggo` 验证 UDP/TCP 53 查询；用 `curl` 验证 DoH GET 与 POST（`terminate` 与 `external` 两种 endpoint）。
3. WebUI 首用户初始化、Management apply 后文件落在卷内且容器重启后仍生效。
4. 记录 bridge 模式下 `peer` 地址的实际取值，并按需验证 4.4 的缓解方式与 host 网络替代路径。
5. `cap_drop`、`no-new-privileges`、宿主目录 root 归属组合下的启动与写入行为。

### 7.3 退出条件

1. 镜像由独立 workflow 在 tag 推送后成功推送，可被 `docker pull` 并在 CI 内通过 7.1 的全部检查；该 workflow 不依赖 Release workflow 的结果。
2. 部署示例可被复制后直接运行（7.2 第 1 项通过）。
3. 长期事实已沉淀到 [交付实现](../implementation/delivery.md)，构建边界已写入 [环境规则](../rules/environment-usage.md)，[README](../../README.md) 入口可用。
4. 未完成的运行验收继续留在活动计划中，不以“代码已实现”替代验收记录。
5. 本方案文档与 `docs/plans/README.md` 索引项在同一交付批次删除。

## 8. 实施状态与剩余验收

已落地的文件：`.github/workflows/container.yml`（独立镜像流水线，`release.yml` 零改动）、[`docker/Dockerfile`](../../docker/Dockerfile)、[`.dockerignore`](../../.dockerignore)、[`docker/compose-example.yaml`](../../docker/compose-example.yaml)、[`docker/.env.example`](../../docker/.env.example)、[`docker/container-smoke-config.yaml`](../../docker/container-smoke-config.yaml)、[`.gitignore`](../../.gitignore) 的 `docker/.env` 忽略项，以及[交付实现](../implementation/delivery.md#容器镜像交付)、[环境规则](../rules/environment-usage.md)与 [README](../../README.md) 的同步更新。

本批次实际执行的本地验证：

1. `.github/workflows/container.yml`、`release.yml`、`docker/compose-example.yaml`、`docker/container-smoke-config.yaml`、`config-example.yaml` 用仓库既有 `js-yaml 4.3.1` 解析通过。
2. Rust 1.98.0 本地 debug 构建成功后，`fluxdns validate` 通过新增冒烟夹具（2 个 listener、1 个 upstream、1 个策略）、根模板与既有启动夹具。
3. 用同一 debug 二进制在临时目录实跑 `run`：`service_ready` 在 2s 内写入 `<work.path>/logs/fluxdns.log`，工作目录自动生成 `data/`、`logs/`，未出现绑定 53 失败。这一步在 Windows 本机执行，不等同容器内验证。

4. 三个容器相关 action 的主版本按 GitHub Releases 页核对后取当前主版本：`docker/setup-buildx-action@v4`（v4.3.0）、`docker/login-action@v4`（v4.6.0）、`docker/build-push-action@v7`（v7.4.0）；未在真实 runner 上执行验证。
5. 文档检查器与 `git diff --check` 通过。

尚未完成、决定退出条件的验收：

1. tag 推送后的真实 Actions 运行：镜像构建、推送 GHCR、`--version`/`validate`/启动冒烟/`docker stop` 与 `docker compose config` 五类镜像级检查。
2. 目标主机上的人工验收（7.2 全部 5 项）。
3. 镜像体积、容器内性能、GHCR 包可见性与 `latest` 拉取行为未测量。

在这些验证通过前，本方案与 `docs/plans/README.md` 索引项保留，状态为待验收。
