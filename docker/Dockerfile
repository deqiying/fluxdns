# syntax=docker/dockerfile:1
#
# FluxDNS 运行镜像：只包含二进制与运行期依赖，编译在 GitHub Actions 中完成
# （见 .github/workflows/container.yml），镜像内不做任何编译。
FROM debian:trixie-slim

# 远程规则集与 DoH 上游需要可信 CA。
RUN apt-get update \
 && apt-get install --yes --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

ARG FLUXDNS_VERSION=0.0.0
ARG FLUXDNS_REVISION=unknown

# 仓库没有 LICENSE，因此不声明 org.opencontainers.image.licenses。
LABEL org.opencontainers.image.title="FluxDNS" \
      org.opencontainers.image.description="A policy-driven DNS server" \
      org.opencontainers.image.source="https://github.com/deqiying/fluxdns" \
      org.opencontainers.image.version="${FLUXDNS_VERSION}" \
      org.opencontainers.image.revision="${FLUXDNS_REVISION}"

# 二进制由工作流编译后暂存到构建上下文的 deploy/ 目录；该目录不进版本库。
COPY deploy/fluxdns /usr/local/bin/fluxdns

# 工作目录固定为 /etc/fluxdns：它与配置里的 work.path 一致，源配置即
# <work.path>/config.yaml，因此启动不会生成第二份配置快照。
RUN install -d -o 0 -g 0 -m 0750 /etc/fluxdns

# 默认以 root 运行，才能绑定配置中的 53/443；能力与 no-new-privileges 由部署层收紧，
# 见 docker/compose-example.yaml。
WORKDIR /etc/fluxdns
EXPOSE 53/udp 53/tcp 443/tcp 8080/tcp
ENTRYPOINT ["/usr/local/bin/fluxdns"]
CMD ["run", "--config", "/etc/fluxdns/config.yaml"]
