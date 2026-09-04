<h1 align="center">Lantunnel</h1>

<p align="center">
  <strong>把你的私有网络随身带走。</strong><br>
  在任何地方访问自己内网里的机器和服务 —— 优先点对点直连，端到端加密，
  不用做端口映射，也不用把任何东西挂到公网。
</p>

<p align="center">
  <a href="https://github.com/lantunnel/lantunnel/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/lantunnel/lantunnel/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://lantunnel.app/"><img alt="Website" src="https://img.shields.io/badge/website-lantunnel.app-2563eb"></a>
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue"></a>
  <img alt="Rust" src="https://img.shields.io/badge/rust-1.89%2B-orange">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Windows%20%7C%20Linux%20%7C%20Android%20%7C%20iOS-lightgrey">
</p>

<p align="center">
  <a href="https://lantunnel.app/">官网</a> ·
  <a href="https://lantunnel.app/download">下载</a> ·
  <a href="./docs/USAGE.zh-CN.md">使用指南</a> ·
  <a href="./CONTEXT.md">架构文档</a> ·
  <a href="./docs/PROTOCOL.md">协议规范</a>
</p>

<p align="center">
  <a href="./README.md">English</a> ·
  <b>简体中文</b> ·
  <a href="./README.zh-TW.md">繁體中文</a> ·
  <a href="./README.ja.md">日本語</a> ·
  <a href="./README.es.md">Español</a> ·
  <a href="./README.de.md">Deutsch</a> ·
  <a href="./README.fr.md">Français</a>
</p>

---

NAS 在家里。跑模型的那台机器在公司。`ollama` 装在你出门时留在桌上的台式机里。它们全都躲在 NAT 后面，而且一台都不该暴露到公网上。

Lantunnel 把这些机器组成一个小小的私有网络 —— 一条 **Tunnel** —— 只有拿到你签发的配置文件的人才进得来。网络条件允许时，各个节点之间**直连**；直连打不通，就退回到经过 Gateway 的**加密中继**，而 Gateway 转发的是它自己也解不开的密文。两条路都一样：什么都不用对外发布，路由器上不用开一个端口，中间也没有任何一环能看到明文。

> ### 🚀 不想自己搭 Gateway？那就别搭。
>
> **[lantunnel.app](https://lantunnel.app/)** 给每个账号一条**永久免费的 Tunnel** —— 点对点流量不限量，每个 Client 后面挂多少台内网设备都不限，另外每月 5 GB 的加密中继额度，留给直连打不通的时候用。建一条 Tunnel、下载 Client、导入配置文件，完事。不用服务器，不用证书，不用配 DNS。
>
> 想自己托管 Gateway 也行 —— 全套代码就在这个仓库里，Apache-2.0 协议，而且完全不计量。
>
> **[→ 领取你的免费 Tunnel](https://lantunnel.app/)**

---

## 它能给你什么

| | |
|---|---|
| **直连优先** | 新连接先尝试点对点 QUIC 直连，配合 UDP 打洞。中继是兜底方案，不是默认路径。 |
| **端到端加密** | 中继流量用 XChaCha20-Poly1305 封装，密钥来自两个 Peer 之间的 X25519 协商。Gateway 转发的是它解不开的字节。 |
| **不用端口映射** | 所有 Peer 都是主动往外拨号。你内网里的任何东西都不需要入站规则、公网 IP 或者域名。 |
| **整个内网都能访问** | 一个 Peer 可以把自己所在的私有网段发布出去 —— 网络里放一个 Client，NAS、打印机、监控面板就都能被 Tunnel 里的其他人访问到。 |
| **访问控制权在你手上** | 每个 Client 自己决定对外提供什么。策略存在被访问的那台机器上 —— 不在 Gateway 上，也不在任何服务器上。 |
| **一个程序，带界面或不带** | `lantunnel-client` 默认打开桌面窗口，加上 `--headless` 就是同一套运行时，跑在服务器上。 |
| **平台齐全** | macOS、Windows、Linux、Android、iOS。 |

### 大家实际拿它做什么

- **游戏和影音串流** —— 访问家里那台机器上的 Sunshine/Moonlight、Jellyfin、Plex。
- **私有 AI 和开发工具** —— Ollama、Open WebUI、内部 API、测试环境、绝对不能出内网的数据库。
- **家庭和办公服务** —— NAS、Home Assistant、摄像头、内部看板、SSH。

## 工作原理

```
┌─────────────────── 一条 Tunnel ───────────────────┐
│                                                   │
│  ┌────────────┐                   ┌────────────┐  │
│  │   Peer A   │◀─── QUIC 直连 ───▶│   Peer B   │  │
│  │   笔记本   │     （优先）      │  家里 NAS  │  │
│  └──────┬─────┘                   └──────┬─────┘  │
│         │                                │        │
└─────────┼────────────────────────────────┼────────┘
          │    ┌────────────────────┐      │
          └───▶│      Gateway       │◀─────┘
    加密中继   │  会合点 +          │  加密中继
    （兜底）   │  NAT 穿透信令 +    │  （兜底）
               │  不透明转发        │
               └────────────────────┘
                   它只看得到密文
```

整个系统就三个部分：

- **`lantunnel-client`** 装在每台加入的设备上。导入一份签名过的 `.peer` 配置文件，连上 Gateway，然后在本地开一个 SOCKS5 代理，也可以选择安装系统路由 —— 这样普通程序不用知道 Lantunnel 的存在就能访问到 Tunnel。
- **`lantunnel-gateway`** 是会合点和 NAT 穿透的信令方。它靠一份公开的 `.scope` 文件来放行某条 Tunnel，帮两个 Peer 打通直连，打不通就转发封装好的字节。它不持有任何 Peer 私钥，也看不到明文。
- **`lantunnel-admin`** 离线创建 Tunnel。两条命令：`init-tunnel` 生成 owner 文件和给 Gateway 用的公开 scope，`add-peer` 给每台设备签发一份配置。它不联网。

身份靠签名，不靠共享密码。没有 Tunnel 密码，没有群组密钥，也没有 bearer token —— 每个 Peer 持有自己的 Ed25519 私钥，每次接入都要证明自己拥有它，而这把私钥永远不离开生成它的那台机器。

📖 **[架构与概念 →](./CONTEXT.md)**  ·  📐 **[线格式规范 →](./docs/PROTOCOL.md)**

## 快速上手

### 最快的路 —— 用托管 Gateway

1. 到 **[lantunnel.app](https://lantunnel.app/)** 建一条免费 Tunnel。
2. 给每台设备加一个 Peer，下载对应的 `.peer` 配置文件。
3. 从 **[lantunnel.app/download](https://lantunnel.app/download)** 装上 Client，导入配置文件。

就这样。把程序的代理指到 `127.0.0.1:1080`，或者打开系统路由直接用内网地址访问。

### 自己来 —— 你的 Gateway，你的规则

```bash
# 1. 离线创建 Tunnel。这一步全程不联网。
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443
#   → <tunnel-id>.tunnel   这是 Tunnel 的签名私钥，要保管好
#   → <tunnel-id>.scope    公开文件，Gateway 只需要这个

# 2. 给每台设备签发一份配置。
lantunnel-admin add-peer --tunnel <tunnel-id>.tunnel --name laptop --output laptop.peer
lantunnel-admin add-peer --tunnel <tunnel-id>.tunnel --name nas    --output nas.peer

# 3. 把公开 scope 放到 Gateway 主机上，然后启动。
mkdir -p state/scopes.d && cp <tunnel-id>.scope state/scopes.d/
lantunnel-gateway --config configs/gateway.yaml

# 4. 每台设备导入自己那份配置并连接。
lantunnel-client tunnel import ./laptop.peer
lantunnel-client                          # 桌面界面
lantunnel-client connect '<tunnel_id>'    # 同一套运行时，不开窗口
```

一台设备一份配置 —— `.peer` 不是拿来到处复制的。

📘 **[完整使用指南 —— 安装、内网发布、访问规则、服务器部署、手机端、故障排查 →](./docs/USAGE.zh-CN.md)**

## 仓库里有什么

自己跑起 Lantunnel 所需的一切，全部 Apache-2.0：

| 路径 | 内容 |
|---|---|
| `apps/lantunnel-client` | Client。Tauri 桌面界面 + headless 运行时，同一个二进制。 |
| `apps/lantunnel-gateway` | Gateway。 |
| `apps/lantunnel-admin` | 离线签发工具：`init-tunnel`、`add-peer`。 |
| `apps/android-proxy` | Android 应用（VpnService）。 |
| `apps/ios-proxy` | iOS 应用（NetworkExtension）。 |
| `crates/tp-*` | 共享实现 —— 协议、传输层、代理、P2P、Gateway 与 Client 引擎。 |
| `docs/PROTOCOL.md` | 线格式规范（规范性文档）。 |
| `CONTEXT.md` | 架构与术语。 |
| `docs/USAGE.zh-CN.md` | 怎么用。 |

lantunnel.app 上的托管平台 —— 账号、计费、托管 Gateway 集群 —— 是一个独立的闭源服务，**不在**这个仓库里。这里的代码不依赖它。自托管部署全程不会联系它。

## 从源码构建

需要 Rust 1.89+、`protoc`（gRPC 传输用）、Node（构建 Client 前端）。

```bash
# Gateway 和签发工具
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin

# Client（先构建前端）
npm --prefix apps/lantunnel-client/frontend ci
npm --prefix apps/lantunnel-client/frontend run build
cargo build --release -p lantunnel-client
```

Linux 上 Client 会链接 webkit2gtk、appindicator 和 rsvg；具体要装哪些 `-dev` 包见 [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)。

检查项，以及一套三 Peer 端到端验收 —— 它会先走直连、再走加密中继，把每个方向的 TCP 和 UDP 组合各验一遍：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e/v2_docker/run.sh
```

## 版本兼容性

Peer、Gateway 和配置文件必须来自同一条 2.0.x 线 —— 线格式不做跨版本协商。从 1.x 升上来？旧的配置文件导不进来，用 `lantunnel-admin` 重新签发。

## 参与贡献

欢迎提 issue 和 PR —— 构建、测试和代码风格见 [CONTRIBUTING.md](./CONTRIBUTING.md)。发现安全漏洞？请按 [SECURITY.md](./SECURITY.md) 走私密上报，不要开公开 issue。

## 许可证

Apache License 2.0 —— 见 [LICENSE](./LICENSE) 和 [NOTICE](./NOTICE)。

---

> 本文是英文 [README.md](./README.md) 的中文版。两者出现分歧时，以英文版为准。

<p align="center">
  <strong>不想折腾？</strong> 一条永久免费 Tunnel，直连流量不限，托管 Gateway 随时待命。<br>
  <a href="https://lantunnel.app/"><strong>到 lantunnel.app 开始 →</strong></a>
</p>
