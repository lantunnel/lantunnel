# Lantunnel 使用指南

一份实操手册：连上、访问到你的机器、并且守住谁能访问你。

刚接触这个项目？先看 [README](../README.zh-CN.md)。想了解背后的设计？看 [CONTEXT.md](../CONTEXT.md)。

[English](./USAGE.md) ·
**简体中文** ·
[繁體中文](./USAGE.zh-TW.md) ·
[日本語](./USAGE.ja.md) ·
[Español](./USAGE.es.md) ·
[Deutsch](./USAGE.de.md) ·
[Français](./USAGE.fr.md)

**目录**

1. [一分钟讲清楚](#一分钟讲清楚)
2. [选一条路](#选一条路)
3. [路线 A —— 托管 Gateway（最快）](#路线-a--托管-gateway最快)
4. [路线 B —— 自托管 Gateway](#路线-b--自托管-gateway)
5. [怎么访问](#怎么访问)
6. [把整个内网共享出去](#把整个内网共享出去)
7. [决定谁能访问你](#决定谁能访问你)
8. [跑在服务器上（headless）](#跑在服务器上headless)
9. [手机端](#手机端)
10. [命令速查](#命令速查)
11. [配置项速查](#配置项速查)
12. [文件都在哪](#文件都在哪)
13. [故障排查](#故障排查)

---

## 一分钟讲清楚

一条 **Tunnel** 就是一小组互相信任的机器。里面每台机器叫一个 **Peer**，每个 Peer 持有一份签名过的 **`.peer` 配置文件** —— 它包含这台设备的身份、私钥，以及怎么找到 Gateway。

两个 Peer 只要在同一条 Tunnel 里，网络允许时就直接对话。不允许时，就退回经由 **Gateway** 的中继，而 Gateway 转发的是它自己也解不开的密文。Gateway 是个会合点，不是中间人。

三样东西你永远不需要：内网里的公网 IP、路由器上的端口映射、共享密码。

## 选一条路

|  | **托管 Gateway** | **自托管 Gateway** |
|---|---|---|
| 你要跑 | 只跑 Client | Client + 你自己的 Gateway |
| 你需要 | 一个 [lantunnel.app](https://lantunnel.app/) 账号 | 一台有公网地址的机器 |
| 配置耗时 | 几分钟 | 约 20 分钟 |
| 中继流量 | 每月 5 GB 免费，超出计量 | 你自己的，不计量 |
| 点对点直连 | 不限量 | 不限量 |

两条路用的是同一个 Client、同一套协议。可以先用托管的，之后再搬 —— 也可以两个都跑，因为 Tunnel 本身跟任何账号都无关。

---

## 路线 A —— 托管 Gateway（最快）

**[lantunnel.app](https://lantunnel.app/)** 替你运行整个 Gateway 集群。每个账号有一条永久免费的 Tunnel：点对点流量不限，每个 Client 后面的内网设备数不限，外加每月 5 GB 加密中继，留给直连打不通的时候。

1. **建一条 Tunnel** —— 到 [lantunnel.app](https://lantunnel.app/) 注册并创建免费 Tunnel。不用填 Gateway 地址，不用证书，不用配 DNS。
2. **给每台设备加一个 Peer** —— 笔记本一个，NAS 一个，台式机一个。分别下载 `.peer` 配置文件。
3. **装 Client** —— 从 [lantunnel.app/download](https://lantunnel.app/download) 下载，或者从本仓库自行构建。
4. **导入并连接：**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # 打开界面，在里面点连接
   ```

托管模式的配置文件里只有平台地址。连接时 Client 会去问自己这条 Tunnel 当前在哪个 Gateway 上，用自己的私钥签名请求，然后拿回连接信息。换 Gateway 的时候，你的设备上什么都不用改。

可以直接跳到[怎么访问](#怎么访问)。

---

## 路线 B —— 自托管 Gateway

下面用到的东西全在这个仓库里，Apache-2.0，全程不联系 lantunnel.app。

### 你需要准备

- 一台公网能访问到的机器 —— 5 美元的 VPS 完全够用；Gateway 主要做信令，中继只承载直连打不通的那部分流量。
- 上面开两条入站规则：**数据端口**（TCP 还是 UDP 取决于传输方式）和 **UDP 映射端口**（默认 `8444`）。
- 一张 TLS 证书。正式签发的，或者你自己签发并固定指纹的自签名证书，都行。

### 1. 构建（或下载）二进制

```bash
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin
```

### 2. 给 Gateway 配证书

有域名的正式证书直接用即可。自签名的话：

```bash
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout certs/server.key -out certs/server.crt \
  -subj "/CN=gw.example.com" \
  -addext "subjectAltName = DNS:gw.example.com" \
  -addext "basicConstraints = critical, CA:FALSE" \
  -addext "keyUsage = critical, digitalSignature, keyEncipherment" \
  -addext "extendedKeyUsage = serverAuth"
chmod 0600 certs/server.crt certs/server.key
```

没有域名的话，把 DNS SAN 换成 **IP SAN**（`IP:203.0.113.10`）。

### 3. 离线创建 Tunnel

`lantunnel-admin` 全程不联网，在哪台机器上跑都行。它生成的 `.tunnel` 文件是这条 Tunnel 的签名私钥，请妥善保管。

```bash
mkdir -p provision
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443 \
  --gateway-cert certs/server.crt \
  --output-dir ./provision
```

它会写出两个以 Tunnel ID 命名的文件：

| 文件 | 给谁 | 内容 |
|---|---|---|
| `<tunnel-id>.tunnel` | **只给你自己。** 权限 `0600`。 | Tunnel 的签名私钥。丢了就再也签发不了新 Peer；泄露了别人就能替你签发。 |
| `<tunnel-id>.scope` | 给 Gateway。公开。 | 只有 Tunnel ID 和签名**公**钥。它签发不了 Peer，也读不了流量。 |

`init-tunnel` 的选项：

- `--gateway-transport quic | websocket | grpc` —— QUIC 是首选，也是唯一支持每条流独立通道的。WebSocket 和 gRPC 用于封锁 UDP 的网络环境。
- `--gateway-host` 和/或 `--gateway-ip` —— 两个都给的话，拨号用 IP，域名用作 TLS 服务器名。
- `--gateway-cert` —— 要固定的 PEM 证书。Gateway 用的是公信 CA 证书时可以省略。

### 4. 每台设备签发一份配置

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

每次 `add-peer` 都会从 `198.18.0.0/16` 里分配一个 **Overlay IP**，生成新密钥对，签名成员身份，并原子地更新 owner 文件。

> **一台设备一份 `.peer`。** 把配置复制到第二台机器不会克隆出一个 Peer —— 两个实例会争抢同一个身份，Gateway 会拒掉后来的那个。

常用参数：`--overlay-ip` 指定地址，`--replicas` 允许该 Peer 同时建立多条传输连接。

### 5. 启动 Gateway

**只把公开的 scope** 拷到 Gateway 主机上：

```bash
mkdir -p state/scopes.d
cp ./provision/<tunnel-id>.scope state/scopes.d/
```

用一份基于 [`configs/gateway.yaml`](../configs/gateway.yaml) 的配置启动：

```bash
lantunnel-gateway --config configs/gateway.yaml
```

关键几项：

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # 必须和 --gateway-port 一致
  transport_type: "quic"          # 必须和 --gateway-transport 一致
  tls_cert: "certs/server.crt"
  tls_key: "certs/server.key"
  scopes_dir: "state/scopes.d"    # .scope 文件放这里
  mapping_probe_port: 8444        # UDP；Gateway 自己绑定
```

Gateway 自己绑定映射端口 —— 没有第二个进程要启动。同一台主机上跑多个 Gateway 的话，每个都要有自己的数据端口**和**自己的映射端口。QUIC 数据监听不能和映射端口共用。

以后要加新的 Tunnel，往 `scopes_dir` 里再丢一个 `.scope` 就行。systemd 示例单元在 [`scripts/remote/`](../scripts/remote/)。

### 6. 各设备连接

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # 确认；不会打印私钥
lantunnel-client                      # 图形界面
lantunnel-client connect <tunnel-id>  # 或者无界面运行
```

---

## 怎么访问

连上之后，有两种方式把流量送进 Tunnel。

### 1. 本地 SOCKS5 代理 —— 一直开着

每个连上的 Client 都会在 **`127.0.0.1:1080`** 开一个 SOCKS5 代理，只监听回环，不需要认证。它不需要认证：它绑在回环地址上，而且经过它的每一个请求都要过**目标** Peer 自己的策略。

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # 某个 Peer 上的 Jellyfin
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # 某个 Peer 内网里的 NAS
```

浏览器、`ssh -o ProxyCommand`、Docker，以及大多数命令行工具都直接支持 SOCKS5 代理。`1080` 被占用的话，用 `--local-socks5-listen 127.0.0.1:1081` 换个端口。

Client 连接状态下，桌面设置面板里可以一键复制这个监听地址对应的 Clash YAML 片段。

### 2. 系统路由 —— 所有程序，零配置

打开系统路由后，这台机器会为 Tunnel 安装真实路由，于是**任何**程序都能按地址访问到其他 Peer，完全不需要知道 Lantunnel 的存在。

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

也可以在桌面界面里切换网络模式并添加路由。手机上没有这个选项 —— VPN 服务是唯一能接管其他 App 流量的方式，所以它始终生效。

**Tunnel First** 决定了：当远端 Tunnel 路由和你当前物理所在的网段重叠时，谁说了算。关闭（默认）时本地内网优先；打开时 Tunnel 优先 —— 在咖啡厅 Wi-Fi 恰好也用 `192.168.1.0/24` 的时候很有用。无论开关如何，Gateway、控制通道、DNS 和自身导出的目标始终走本机路由。

### 我该用哪个地址？

| 要访问 | 用 |
|---|---|
| 远端 Peer 机器上自己跑的服务 | 它的 **Overlay IP**（`198.18.x.y`）加服务端口。`lantunnel-client tunnel list` 会以 JSON 打印，界面上也能看到。 |
| 远端 Peer 内网里的某台设备 | 那台设备的**真实内网地址**，比如 `192.168.1.50`。 |

默认情况下，Overlay 上的某个端口映射到目标机器上 `127.0.0.1` 的同一端口。

---

## 把整个内网共享出去

一个 Peer 可以对外通告自己所在的私有网段。这样其他 Peer 就能通过它访问那些网段上的**任何**设备 —— NAS、打印机、交换机的网页后台 —— 而那些设备上什么都不用装。

两个互相独立的来源，界面上默认都是开的：

- **导出当前内网**（`auto_export_current_lan`，默认开启）会把这台机器当前接入的私有网段发布出去，并且每次扫描网卡时重新计算。笔记本从家里带到公司，导出的网段就跟着变。
- **手动填写的导出**（`exported_lans`）是你自己指定的网段。

关掉自动开关只会撤回它自己加进去的那部分，你手填的列表不受影响。

只接受 RFC1918 的 IPv4 网段。默认路由、公网地址段、回环、链路本地、组播，以及任何和 Overlay 地址池重叠的网段，都会被拒绝。

**导出只是让对方能连到，不等于允许连。** 远端 Peer 访问每个目标时，仍然要过导出方 Client 的[访问策略](#决定谁能访问你)。

如果两个 Peer 导出了同一个网段，每个 Client 会选自己最先看到的那个，等它最后一条路径也断了才切到下一个。这是各 Client 自己的选择，不持久化，所以你的两台机器选到不同的导出方是正常的。

---

## 决定谁能访问你

**Client 访问策略**是 Lantunnel 里唯一的 ACL，而且它存在被访问的那台机器上。不在 Gateway 上，也不在任何服务器上。路由选择决定**往哪儿发**；你的 Client 独立决定**要不要提供服务**。

默认行为：空策略意味着**持有你这条 Tunnel 配置文件的每个 Peer 都能访问你**。能拿到配置文件本身就得由你签发，所以在这之上再加一道门并不会带来额外的边界 —— 只会让新装的 Client 莫名其妙连不上。一旦你写了第一条 Allow 规则，它就成了唯一入口。**Deny 永远最先检查，永远优先。**

在桌面界面里设置，或者直接改 `settings.json`：

```jsonc
{
  "client_access": {
    "allow": [
      // 允许 SSH 到本机
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // 允许访问旁边 NAS 上的 Jellyfin
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // 允许 IoT 网段上的任意 TCP 端口
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // ……但路由器永远不许碰，不管 Allow 里写了什么
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

目标类型有 `this_peer`、`ip`、`cidr`、`host`。端口是 `any` 或 `exact` —— 不支持端口范围。规则顺序没有意义，只有「Deny 压过 Allow」这一条。规则里永远不能指定来源 Peer：Tunnel 里每个通过认证的成员得到的结果都一样。

要彻底关闭对外服务，就对 `0.0.0.0/0` 和 `::/0` 在 TCP 和 UDP 上都写 Deny —— 界面上的「阻止所有入站」写进去的就是这个，所以存下来的内容和你要求的完全一致。

---

## 跑在服务器上（headless）

`--headless`（别名 `--no-ui`）运行的是完全相同的运行时，只是没有窗口、托盘和 WebView —— 重连逻辑一样，PeerLink 和中继行为一样，SOCKS5 和 TUN 也一样。

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # 前台运行，无界面
lantunnel-client status --json                # 另开一个终端查看
lantunnel-client disconnect
```

单独用 `--headless` 会连接标记为自动连接的那份配置，所以服务单元里不用写 Tunnel ID：

```ini
[Unit]
Description=Lantunnel Client
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/lantunnel-client --headless
Restart=always
RestartSec=5
User=lantunnel
Environment=TUNNEL_PROXY_APP_CONFIG_DIR=/var/lib/lantunnel

[Install]
WantedBy=multi-user.target
```

headless 模式没有设置界面，直接改配置目录里的 `settings.json` —— 见[配置项速查](#配置项速查)。

**Windows 上**，正式构建使用 GUI 子系统，所以正常启动不会弹出控制台窗口，`cmd.exe` 也不会等它结束。当你在意某条短命令的输出和退出码时，用 `start /wait`：

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## 手机端

Android（`apps/android-proxy`，VpnService）和 iOS（`apps/ios-proxy`，NetworkExtension）通过 `tp-mobile-ffi` 跑的是同一套 Rust 内核。扫描 `.peer` 配置文件的二维码，或者直接打开文件导入，然后启动 VPN。

手机上没有网络模式开关：VPN 服务是唯一能接管其他 App 流量的方式，所以系统路由始终跟随运行时。

---

## 命令速查

### `lantunnel-client`

```
lantunnel-client                          打开桌面界面
lantunnel-client connect <TUNNEL_ID>      连接一份已导入的配置，无界面
lantunnel-client disconnect               断开正在运行的 Client
lantunnel-client status --json            以 JSON 打印状态
lantunnel-client tunnel import <FILE>     导入一份 .peer 配置
lantunnel-client tunnel list              以 JSON 列出已导入的配置
```

`tunnel list` 会打印每份配置的 Tunnel ID、Peer ID、Overlay IP 和引导方式。私钥材料不可序列化，永远不会出现。

| 选项 | 含义 |
|---|---|
| `--headless`、`--no-ui` | 运行完整运行时但不开界面 |
| `--log-level <LEVEL>` | `error`、`warn`、`info`、`debug`、`trace` |
| `--local-socks5-listen <ADDR>` | 更换回环 SOCKS5 监听地址 |
| `--desktop-network-mode <MODE>` | `socks5_only` 或 `lan_routes_tun` |
| `--lan-route <CIDR>` | 安装一条系统内网路由（可重复） |
| `--enable-lan-p2p` | 允许把内网地址作为直连候选 |
| `-V`、`--help` | 版本、帮助 |

环境变量覆盖：`LANTUNNEL_LOCAL_SOCKS5_LISTEN`、`LANTUNNEL_DESKTOP_NETWORK_MODE`、`LANTUNNEL_LAN_ROUTES`、`TUNNEL_PROXY_APP_CONFIG_DIR`。

### `lantunnel-admin`

```
lantunnel-admin init-tunnel --gateway-transport <quic|websocket|grpc>
                            [--gateway-host <HOST>] [--gateway-ip <IP>]
                            --gateway-port <PORT>
                            [--gateway-cert <PEM>]
                            [--output-dir <DIR>]

lantunnel-admin add-peer --tunnel <FILE.tunnel>
                         [--overlay-ip <IPV4>] [--replicas <N>]
                         [--name <NAME>] [--output <FILE.peer>]
```

设计上就是离线的。它拒绝符号链接，也不会覆盖已存在的文件。

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>]              运行 Gateway
lantunnel-gateway onboard --pairing <FILE>       接入平台托管的 Gateway
lantunnel-gateway mapping serve                  独立的 UDP 映射反射器
```

`--config` 默认是 `configs/gateway.yaml`。`mapping serve` 是给特殊部署结构准备的；正常的 Gateway 自己绑定映射端口，用不到它。

---

## 配置项速查

Client 配置目录下的 `settings.json`。每一项都是可选的。

| 键 | 默认值 | 含义 |
|---|---|---|
| `auto_start` | `false` | 开机自启 |
| `auto_connect` | `false` | 启动后自动连接 |
| `local_proxy_enabled` | `true` | 运行本地 SOCKS5 监听 |
| `local_socks5_listen` | `"127.0.0.1:1080"` | 监听地址（仅回环） |
| `desktop_network_mode` | `"socks5_only"` | 或 `"lan_routes_tun"` 启用系统路由 |
| `lan_routes` | `[]` | `lan_routes_tun` 模式下要安装的路由 |
| `tunnel_first` | `false` | 让 Tunnel 路由压过重叠的本地内网路由 |
| `exported_lans` | `[]` | 本 Peer 对外发布的私有网段 |
| `auto_export_current_lan` | `true` | 同时发布本机当前接入的网段 |
| `client_access` | 开放 | 访问 ACL —— 见[上文](#决定谁能访问你) |
| `p2p_allow_lan_candidates` | `false` | 把内网地址作为直连候选提供出去 |
| `log_level` | `"info"` | Client 日志级别 |

未知的键会被拒绝而不是忽略，所以拼错了会报错，不会静默失效。

---

## 文件都在哪

| 是什么 | 路径 |
|---|---|
| Client 配置、已导入的配置文件、密钥 | `~/.lantunnel/app/`（可用 `TUNNEL_PROXY_APP_CONFIG_DIR` 覆盖） |
| Client 设置 | `~/.lantunnel/app/settings.json` |
| Gateway 配置 | `configs/gateway.yaml`（或 `--config` 指定） |
| Gateway 的 Tunnel 放行文件 | `state/scopes.d/*.scope` |
| Gateway 中继用量账本 | `state/relay-usage.wal` |
| Tunnel owner 文件 | `init-tunnel --output-dir` 指定的位置 —— 记得备份 |

导入的私钥存放在 Client 创建的仅属主可读文件里；它不会写进日志，不会发给 Gateway，也不会离开这台机器。

---

## 故障排查

**Client 连不上。**
先确认 Gateway 在运行，且它的数据端口从外部能访问到（`nc -z gw.example.com 8443`，QUIC 用 `nc -zu`）。然后确认这条 Tunnel 的 `.scope` 在 Gateway 的 `scopes_dir` 里 —— 没有它，Gateway 没有任何理由放你进来。

**提示「Peer already attached」，或者某个 Client 老是被踢。**
两个 Client 在用同一份 `.peer`。用 `add-peer` 再签一份；配置文件是一台设备的身份，不是共享凭据。

**能通，但一直走中继。**
看界面上的流量计数器 —— 它把直连和中继分开统计。两端都是对称 NAT 时打洞会失败。如果两个 Peer 在同一个内网里，加上 `--enable-lan-p2p` 让本地地址也作为候选。另外确认 UDP `8444` 能到达 Gateway；没有映射探测，两边都学不到自己的公网映射。

**直连能用但中继不行（或者反过来）。**
这是两条互相独立的路径。中继依赖 Gateway 的数据端口；直连依赖 UDP 能在两个 Peer 之间打通。一次只测一条。

**远端服务拒绝连接。**
是目标 Client 的访问策略拒绝的 —— 去检查**那台机器**上的 `client_access`，不是你这台。`NotAuthorized` 是终态，不会再去尝试别的 Peer。

**导出的内网访问不到。**
导出方的 Client 必须当前确实接入了那个网段，导出才处于就绪状态 —— 配置的网段只有和已连接的网段完全一致时才会被发布。确认之后，再检查那台 Client 的访问策略是否放行了具体的目标和端口。

**版本不匹配。**
Peer、Gateway 和配置文件必须在同一条 2.0.x 线上。线格式不做跨版本协商，混版本部署会直接失败。

**想看更多细节。**
Client 用 `--log-level debug`，Gateway 配置里设 `log.level: debug`。日志里不会出现私钥、配置文件内容或会话密钥。

---

## 接下来看什么

- **[lantunnel.app](https://lantunnel.app/)** —— 免费 Tunnel、托管 Gateway、下载，以及游戏串流、私有 AI 工具、家庭服务的专题指南。
- **[CONTEXT.md](../CONTEXT.md)** —— 各部分如何组合，以及每个术语的确切含义。
- **[PROTOCOL.md](./PROTOCOL.md)** —— 线格式规范，需要对接实现时看这个。
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** —— 构建、测试、提交补丁。

---

> 本文是英文 [USAGE.md](./USAGE.md) 的中文版。两者出现分歧时，以英文版为准。
