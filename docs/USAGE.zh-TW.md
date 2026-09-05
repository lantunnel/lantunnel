# Lantunnel 使用指南

一份實作手冊：連上線、連到你的機器，並且守住誰能連到你。

剛接觸這個專案？先看 [README](../README.zh-TW.md)。想了解背後的設計？看 [CONTEXT.md](../CONTEXT.md)。

[English](./USAGE.md) ·
[简体中文](./USAGE.zh-CN.md) ·
**繁體中文** ·
[日本語](./USAGE.ja.md) ·
[Español](./USAGE.es.md) ·
[Deutsch](./USAGE.de.md) ·
[Français](./USAGE.fr.md)

**目錄**

1. [一分鐘講清楚](#一分鐘講清楚)
2. [選一條路](#選一條路)
3. [路線 A —— 託管 Gateway（最快）](#路線-a--託管-gateway最快)
4. [路線 B —— 自行託管 Gateway](#路線-b--自行託管-gateway)
5. [怎麼連線](#怎麼連線)
6. [把整個內網分享出去](#把整個內網分享出去)
7. [決定誰能連到你](#決定誰能連到你)
8. [跑在伺服器上（headless）](#跑在伺服器上headless)
9. [手機端](#手機端)
10. [指令速查](#指令速查)
11. [設定項速查](#設定項速查)
12. [檔案放在哪](#檔案放在哪)
13. [疑難排解](#疑難排解)

---

## 一分鐘講清楚

一條 **Tunnel** 就是一小群互相信任的機器。裡面每台機器叫一個 **Peer**，每個 Peer 持有一份簽章過的 **`.peer` 設定檔** —— 內含這台裝置的身分、私鑰，以及如何找到 Gateway。

兩個 Peer 只要在同一條 Tunnel 裡，網路允許時就直接對話。不允許時，就退回經由 **Gateway** 的中繼，而 Gateway 轉送的是連它自己都解不開的密文。Gateway 是會合點，不是中間人。

三樣東西你永遠不需要：內網裡的公網 IP、路由器上的連接埠轉發、共用密碼。

## 選一條路

|  | **託管 Gateway** | **自行託管 Gateway** |
|---|---|---|
| 你要跑 | 只跑 Client | Client + 你自己的 Gateway |
| 你需要 | 一個 [lantunnel.app](https://lantunnel.app/) 帳號 | 一台有公網位址的機器 |
| 設定耗時 | 幾分鐘 | 約 20 分鐘 |
| 中繼流量 | 每月 5 GB 免費，超出計量 | 你自己的，不計量 |
| 點對點直連 | 不限量 | 不限量 |

兩條路用的是同一個 Client、同一套協定。可以先用託管的，之後再搬 —— 也可以兩邊都跑，因為 Tunnel 本身與任何帳號無關。

---

## 路線 A —— 託管 Gateway（最快）

**[lantunnel.app](https://lantunnel.app/)** 替你營運整個 Gateway 叢集。每個帳號有一條永久免費的 Tunnel：點對點流量不限，每個 Client 後面的內網裝置數不限，另外每月 5 GB 加密中繼，留給直連打不通的時候。

1. **建立一條 Tunnel** —— 到 [lantunnel.app](https://lantunnel.app/) 註冊並建立免費 Tunnel。不必填 Gateway 位址、不必準備憑證、不必設定 DNS。
2. **為每台裝置新增一個 Peer** —— 筆電一個、NAS 一個、桌機一個。分別下載 `.peer` 設定檔。
3. **安裝 Client** —— 從 [lantunnel.app/download](https://lantunnel.app/download) 下載，或自行從本儲存庫建置。
4. **匯入並連線：**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # 開啟介面，在裡面按連線
   ```

託管模式的設定檔裡只有平台網址。連線時 Client 會去問自己這條 Tunnel 目前在哪個 Gateway 上，用自己的私鑰簽署請求，然後取回連線資訊。更換 Gateway 時，你的裝置上什麼都不用改。

可以直接跳到[怎麼連線](#怎麼連線)。

---

## 路線 B —— 自行託管 Gateway

以下用到的東西全都在這個儲存庫裡，Apache-2.0，全程不連線到 lantunnel.app。

請嚴格區分兩類機器的用途：

- **Gateway 主機：** 公網機器，存放 Gateway 執行檔、TLS 金鑰對和公開的 `.scope` 檔案。
- **受信任的 owner 機器：** 存放 `lantunnel-admin`、私有的 `.tunnel` owner 檔案，以及每份 `.peer` 檔案；每份 `.peer` 只在傳給對應 Client 前暫存於此。

絕對不要在公網 Gateway 主機上安裝 `lantunnel-admin`，也不要把 `.tunnel` 或 `.peer` 檔案存放在那裡。

### 你需要準備

- 一台公網連得到的機器 —— 5 美元的 VPS 綽綽有餘；Gateway 主要負責信令，中繼只承載直連打不通的那部分流量。
- 上面開兩條輸入規則：**資料連接埠**（TCP 或 UDP，視傳輸方式而定）以及選定的 **UDP 對應連接埠**（預設 `8444`）。
- 一個固定公網 IPv4 或 IPv6 位址。主要流程會自動產生 TLS 身分；網域名稱和公信 CA 憑證屬於後面的進階手動流程。

### 1. 建置（或下載）執行檔

```bash
# Gateway 主機
cargo build --release -p lantunnel-gateway

# 受信任的 owner 機器
cargo build --release -p lantunnel-admin

# 分別完成上面的建置指令後，在對應的建置 shell 中執行。
export PATH="$PWD/target/release:$PATH"
```

[在每台 Peer 裝置上安裝 Lantunnel Client](https://lantunnel.app/download)。若要自行建置 Client，請依照 README 的[從原始碼建置](../README.zh-TW.md#從原始碼建置)一節，依序完成前端和 Rust 建置指令。

### 2. 在 Gateway 主機初始化固定 IP Gateway

請在 Gateway 主機的持久目錄中執行。初始化只在本機進行，不會連線到 lantunnel.app：

```bash
lantunnel-gateway init --public-ip <PUBLIC_IP>
```

預設使用 QUIC/UDP `8443`、對應連接埠 UDP `8444` 和 `configs/gateway.yaml`。可用 `--transport`、`--data-port`、`--mapping-port`、`--config` 變更傳輸、資料連接埠、對應連接埠和設定路徑。

指令會建立 `configs/gateway.yaml`、`certs/server.crt`、`certs/server.key` 和 `state/scopes.d`。在 Linux 和 macOS，私有目錄權限是 `0700`，設定檔、憑證和私鑰是 `0600`。

完全相同的指令再次執行時只會驗證並原樣保留現有檔案。只要指定同一個 `--config` 檔案，再次執行、設定驗證與啟動就不依賴目前的工作目錄。在同一個設定路徑下，IP、傳輸、資料連接埠或對應連接埠不同時會拒絕執行，不會取代 Peer 已固定的 Gateway 身分。同一部署根目錄內的另一個設定檔可以重用同一份相符的憑證。

QUIC 的資料連接埠開放 UDP，WebSocket 和 gRPC 開放 TCP；選定的對應連接埠開放 UDP（預設 `8444`）。`server.key` 永遠留在 Gateway 主機上。

#### 進階流程：網域名稱或公信 CA 憑證

`lantunnel-gateway init` 只處理固定公網 IP 與固定憑證的流程。網域名稱、公信 CA 或私有 CA 部署，請參照 [`configs/gateway.yaml`](../configs/gateway.yaml) 手動準備憑證與設定。

公信 CA 的網域憑證不必用 `--gateway-cert` 固定。憑證鏈和私鑰必須複製成 `certs/server.crt` 與 `certs/server.key` 兩個普通檔案，不能是符號連結；續期後要更新這兩個副本。

自簽網域憑證可這樣產生：

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

手動使用任何憑證，都要把憑證和私鑰設為僅擁有者可讀。需要固定憑證時，只把公開的 `server.crt` 複製到受信任的 owner 機器。

### 3. 離線建立 Tunnel

`lantunnel-admin` 全程不連網。請在受信任的 owner 機器執行；它產生的 `.tunnel` 是 Tunnel 的簽章私鑰，必須留在安全位置。

只把 Gateway 產生的公開 `server.crt` 複製到這台機器，並存成 `certs/server.crt`。私鑰仍留在 Gateway 主機。下面的指令會固定該憑證，並使用與初始化相同的公網 IP、傳輸、資料連接埠和對應連接埠。

```bash
mkdir -p certs provision
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-ip <PUBLIC_IP> \
  --gateway-port 8443 \
  --gateway-mapping-port 8444 \
  --gateway-cert certs/server.crt \
  --output-dir ./provision
```

進階網域名稱流程請改用 `--gateway-host gw.example.com`。若網域憑證由公信 CA 簽發，請省略 `--gateway-cert certs/server.crt`，正常續期就不必重新產生 Peer 設定檔。

它會寫出兩個以 Tunnel ID 命名的檔案：

| 檔案 | 給誰 | 內容 |
|---|---|---|
| `<tunnel-id>.tunnel` | **只給你自己。** 權限 `0600`。 | Tunnel 的簽章私鑰。遺失就再也簽發不了新的 Peer；外流則別人就能代你簽發。 |
| `<tunnel-id>.scope` | 給 Gateway。公開。 | 只有 Tunnel ID 和簽章**公**鑰。它簽發不了 Peer，也讀不到流量。 |

`init-tunnel` 的選項：

- `--gateway-transport quic | websocket | grpc` —— QUIC 是首選，也是唯一支援每條流獨立通道的。WebSocket 與 gRPC 用於封鎖 UDP 的網路環境。
- `--gateway-host` 和／或 `--gateway-ip` —— 兩者都給時，撥號用 IP，網域名稱作為 TLS 伺服器名稱。
- `--gateway-mapping-port` —— Gateway 的 UDP 對應連接埠，預設 `8444`；必須和 `lantunnel-gateway init --mapping-port` 或 `gateway.mapping_probe_port` 一致。
- `--gateway-cert` —— 要固定的 PEM 憑證。Gateway 使用公信 CA 憑證時可省略。

### 4. 每台裝置簽發一份設定檔

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

每次 `add-peer` 都會從 `198.18.0.0/16` 配發一個 **Overlay IP**，產生新的金鑰對，簽署成員身分，並以不可分割的方式更新 owner 檔案。

> **一台裝置一份 `.peer`。** 把設定檔複製到第二台機器不會複製出一個 Peer —— 兩個執行個體會爭搶同一個身分，Gateway 會拒絕後來的那一個。

常用參數：`--overlay-ip` 指定位址，`--replicas` 允許該 Peer 同時建立多條傳輸連線。

### 5. 啟動 Gateway

**只把公開的 scope** 複製到 Gateway 主機上：

```bash
mkdir -p state/scopes.d
cp ./provision/<tunnel-id>.scope state/scopes.d/
```

先驗證自動產生的設定，再啟動 Gateway：

```bash
lantunnel-gateway --config configs/gateway.yaml --check-config
lantunnel-gateway --config configs/gateway.yaml
```

產生的設定會把執行時檔案位置儲存為絕對路徑。下面的 `<DEPLOYMENT_ROOT>` 是持久部署目錄，其中包含設定檔（或它所在的 `configs/` 目錄）以及產生的 `certs/` 和 `state/` 目錄：

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # 必須與 --gateway-port 一致
  transport_type: "quic"          # 必須與 --gateway-transport 一致
  tls_cert: "<DEPLOYMENT_ROOT>/certs/server.crt"
  tls_key: "<DEPLOYMENT_ROOT>/certs/server.key"
  scopes_dir: "<DEPLOYMENT_ROOT>/state/scopes.d"  # .scope 檔案放這裡
  mapping_probe_port: 8444        # UDP；可變更的預設值
```

Gateway 會自己繫結選定的 UDP 對應連接埠，不必啟動第二個行程。QUIC 資料監聽不能和 UDP 對應監聽使用同一個連接埠；WebSocket 和 gRPC 的資料監聽使用 TCP，因此可以使用相同的連接埠號碼。

可以在執行 `init` 後、簽發 Peer 設定檔前修改 `gateway.mapping_probe_port`。接著必須把同一個值傳給 `lantunnel-admin init-tunnel --gateway-mapping-port`，並在防火牆中開放這個 UDP 連接埠。

日後若要變更連接埠，先在防火牆中開放新的 UDP 連接埠，修改 Gateway YAML 中的 `gateway.mapping_probe_port`，然後重新啟動 Gateway。

只修改 Gateway YAML 會造成現有 Peer 設定檔的對應探測失敗。

同時把現有 `.tunnel` 中的 `static_gateway.mapping_port` 以及每份現有 `.peer` 中的 `bootstrap.mapping_port` 更新為同一個值，然後重新匯入 Peer 設定檔並重新連線 Client。

Tunnel ID、已安裝的 `.scope` 和 Peer 成員身分簽章仍然有效，無需建立新 Tunnel、更換 Scope 或重新簽章。

只有原 `.peer` 已遺失時，才需要用同一個 `.tunnel` 執行 `add-peer` 建立新的 Peer 身分。

日後要新增 Tunnel，往 `scopes_dir` 再放一個 `.scope` 就好。systemd 範例單元在 [`scripts/remote/`](../scripts/remote/)。

### 6. 各裝置連線

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # 確認；不會列印私鑰
lantunnel-client                      # 圖形介面
lantunnel-client connect <tunnel-id>  # 或無介面執行
```

---

## 怎麼連線

連上之後，有兩種方式把流量送進 Tunnel。

### 1. 本機 SOCKS5 代理 —— 一直開著

每個連上的 Client 都會在 **`127.0.0.1:1080`** 開一個 SOCKS5 代理，只監聽回送位址，不需要驗證。它不需要驗證：它繫結在回送位址上，而且經過它的每個請求都要通過**目標** Peer 自己的政策。

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # 某個 Peer 上的 Jellyfin
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # 某個 Peer 內網裡的 NAS
```

瀏覽器、`ssh -o ProxyCommand`、Docker，以及大多數命令列工具都直接支援 SOCKS5 代理。`1080` 被佔用的話，用 `--local-socks5-listen 127.0.0.1:1081` 換個連接埠。

Client 連線狀態下，桌面設定面板可以一鍵複製這個監聽位址對應的 Clash YAML 片段。

### 2. 原生路由 —— 所有應用程式，零設定

開啟原生路由後，這台機器會為 Tunnel 安裝真實路由，於是**任何**應用程式都能依位址連到其他 Peer，完全不需要知道 Lantunnel 存在。

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

也可以在桌面介面裡切換網路模式並新增路由。手機上沒有這個選項 —— VPN 服務是唯一能接手其他 App 流量的方式，所以它一律生效。

**Tunnel First** 決定的是：當遠端 Tunnel 路由與你目前實際所在的網段重疊時，誰說了算。關閉（預設）時本地內網優先；開啟時 Tunnel 優先 —— 在咖啡廳 Wi-Fi 剛好也用 `192.168.1.0/24` 時很有用。無論開關如何，Gateway、控制通道、DNS 以及自身匯出的目標一律走本機路由。

### 我該用哪個位址？

| 要連到 | 用 |
|---|---|
| 遠端 Peer 機器上自己跑的服務 | 它的 **Overlay IP**（`198.18.x.y`）加上服務連接埠。`lantunnel-client tunnel list` 會以 JSON 列印，介面上也看得到。 |
| 遠端 Peer 內網裡的某台裝置 | 那台裝置的**實際內網位址**，例如 `192.168.1.50`。 |

預設情況下，Overlay 上的某個連接埠會對應到目標機器上 `127.0.0.1` 的同一個連接埠。

---

## 把整個內網分享出去

一個 Peer 可以對外通告自己所在的私有網段。這樣其他 Peer 就能透過它連到那些網段上的**任何**裝置 —— NAS、印表機、交換器的網頁後台 —— 而那些裝置上什麼都不用裝。

兩個彼此獨立的來源，介面上預設都是開的：

- **匯出目前內網**（`auto_export_current_lan`，預設開啟）會把這台機器目前接上的私有網段發布出去，並在每次掃描網路介面時重新推導。筆電從家裡帶到辦公室，匯出的網段就跟著換。
- **手動填寫的匯出**（`exported_lans`）是你自己指定的網段。

關掉自動開關只會收回它自己加進去的部分，你手填的清單不受影響。

只接受 RFC1918 的 IPv4 網段。預設路由、公網位址範圍、回送位址、連結本機、多播，以及任何與 Overlay 位址池重疊的網段，都會被拒絕。

**匯出只是讓對方連得到，不等於允許連。** 遠端 Peer 存取每個目標時，仍然要通過匯出方 Client 的[存取政策](#決定誰能連到你)。

如果兩個 Peer 匯出了相同網段，每個 Client 會選自己最先看到的那個，等它最後一條路徑也斷了才切換到下一個。這是各 Client 自己的選擇，不會保存下來，所以你的兩台機器選到不同的匯出方是正常的。

---

## 決定誰能連到你

**Client 存取政策**是 Lantunnel 裡唯一的 ACL，而且它存放在被連到的那台機器上。不在 Gateway 上，也不在任何伺服器上。路由選擇決定**往哪裡送**；你的 Client 獨立決定**要不要提供服務**。

預設行為：空政策代表**持有你這條 Tunnel 設定檔的每個 Peer 都能連到你**。能拿到設定檔本身就得由你簽發，所以在這之上再加一道門並不會帶來額外邊界 —— 只會讓剛裝好的 Client 莫名其妙連不上。一旦你寫下第一條 Allow 規則，它就成了唯一入口。**Deny 永遠先檢查，永遠優先。**

在桌面介面裡設定，或直接改 `settings.json`：

```jsonc
{
  "client_access": {
    "allow": [
      // 允許 SSH 到本機
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // 允許存取旁邊 NAS 上的 Jellyfin
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // 允許 IoT 網段上的任意 TCP 連接埠
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // ……但路由器永遠不准碰，不管 Allow 裡寫了什麼
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

目標類型有 `this_peer`、`ip`、`cidr`、`host`。連接埠是 `any` 或 `exact` —— 不支援連接埠範圍。規則順序沒有意義，只有「Deny 壓過 Allow」這一條。規則裡永遠不能指定來源 Peer：Tunnel 中每個通過驗證的成員得到的結果都一樣。

要完全關閉對外服務，就對 `0.0.0.0/0` 和 `::/0` 在 TCP 與 UDP 上都寫 Deny —— 介面上的「封鎖所有輸入」寫進去的就是這個，所以存下來的內容跟你要求的完全一致。

---

## 跑在伺服器上（headless）

`--headless`（別名 `--no-ui`）執行的是完全相同的執行環境，只是沒有視窗、系統匣和 WebView —— 重連邏輯一樣，PeerLink 與中繼行為一樣，SOCKS5 和 TUN 也一樣。

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # 前景執行，無介面
lantunnel-client status --json                # 另開一個終端查看
lantunnel-client disconnect
```

單獨使用 `--headless` 會連線到標記為自動連線的那份設定檔，所以服務單元裡不必寫 Tunnel ID：

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

headless 模式沒有設定介面，直接改設定目錄裡的 `settings.json` —— 見[設定項速查](#設定項速查)。

**Windows 上**，正式版建置使用 GUI 子系統，所以正常啟動不會跳出主控台視窗，`cmd.exe` 也不會等它結束。當你在意某個短指令的輸出與結束代碼時，用 `start /wait`：

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## 手機端

Android（`apps/android-proxy`，VpnService）與 iOS（`apps/ios-proxy`，NetworkExtension）透過 `tp-mobile-ffi` 執行的是同一套 Rust 核心。掃描 `.peer` 設定檔的 QR code，或直接開啟檔案匯入，然後啟動 VPN。

手機上沒有網路模式開關：VPN 服務是唯一能接手其他 App 流量的方式，所以原生路由一律跟隨執行環境。

---

## 指令速查

### `lantunnel-client`

```
lantunnel-client                          開啟桌面介面
lantunnel-client connect <TUNNEL_ID>      連線一份已匯入的設定檔，無介面
lantunnel-client disconnect               中斷執行中的 Client
lantunnel-client status --json            以 JSON 列印狀態
lantunnel-client tunnel import <FILE>     匯入一份 .peer 設定檔
lantunnel-client tunnel list              以 JSON 列出已匯入的設定檔
```

`tunnel list` 會列印每份設定檔的 Tunnel ID、Peer ID、Overlay IP 與啟動方式。私鑰材料無法序列化，永遠不會出現。

| 選項 | 意義 |
|---|---|
| `--headless`、`--no-ui` | 執行完整執行環境但不開介面 |
| `--log-level <LEVEL>` | `error`、`warn`、`info`、`debug`、`trace` |
| `--local-socks5-listen <ADDR>` | 更換回送 SOCKS5 監聽位址 |
| `--desktop-network-mode <MODE>` | `socks5_only` 或 `lan_routes_tun` |
| `--lan-route <CIDR>` | 安裝一條原生內網路由（可重複） |
| `--enable-lan-p2p` | 允許把內網位址作為直連候選 |
| `-V`、`--help` | 版本、說明 |

環境變數覆寫：`LANTUNNEL_LOCAL_SOCKS5_LISTEN`、`LANTUNNEL_DESKTOP_NETWORK_MODE`、`LANTUNNEL_LAN_ROUTES`、`TUNNEL_PROXY_APP_CONFIG_DIR`。

### `lantunnel-admin`

```
lantunnel-admin init-tunnel --gateway-transport <quic|websocket|grpc>
                            [--gateway-host <HOST>] [--gateway-ip <IP>]
                            --gateway-port <PORT>
                            [--gateway-mapping-port <PORT>]
                            [--gateway-cert <PEM>]
                            [--output-dir <DIR>]

lantunnel-admin add-peer --tunnel <FILE.tunnel>
                         [--overlay-ip <IPV4>] [--replicas <N>]
                         [--name <NAME>] [--output <FILE.peer>]
```

設計上就是離線的。它拒絕符號連結，也不會覆寫已存在的檔案。

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>] [--check-config]       執行或驗證 Gateway
lantunnel-gateway init --public-ip <PUBLIC_IP>
                        [--transport <quic|websocket|grpc>]
                        [--data-port <PORT>] [--mapping-port <PORT>]
                        [--config <FILE>]
lantunnel-gateway onboard --pairing <FILE>                接入平台託管的 Gateway
lantunnel-gateway mapping serve                           獨立的 UDP 對應反射器
```

`init` 不連網，用來初始化獨立的固定 IP Gateway。預設使用 QUIC/8443、對應 UDP `8444`，並寫入 `configs/gateway.yaml`；可用 `--mapping-port` 選擇其他對應連接埠。完全相同的指令會保留現有設定與身分；只要指定同一個設定檔，再次執行、驗證與啟動就不依賴目前的工作目錄。在同一個設定路徑下，IP、傳輸、資料連接埠或對應連接埠變更時會拒絕執行。同一部署根目錄內的另一個設定檔可以重用同一份相符的憑證。

網域名稱和公信 CA 部署使用上面的進階手動流程。所有模式的 `--config` 預設都是 `configs/gateway.yaml`。`mapping serve` 只供特殊部署使用；一般 Gateway 會自己繫結對應連接埠。

平台託管接入必須從全新且僅擁有者可存取的工作目錄開始，Gateway 才能安全寫入執行階段設定。請依照[平台連線型 Gateway 安裝指南](https://lantunnel.app/docs/installation#platform-connected)，或使用相同的安全步驟：

```bash
mkdir -m 700 lantunnel-gateway-state
mv /path/to/downloaded-pairing.yaml lantunnel-gateway-state/pairing.yaml
chmod 600 lantunnel-gateway-state/pairing.yaml
cd lantunnel-gateway-state
lantunnel-gateway onboard --pairing pairing.yaml
```

---

## 設定項速查

Client 設定目錄下的 `settings.json`。每一項都是選填的。

| 鍵 | 預設值 | 意義 |
|---|---|---|
| `auto_start` | `false` | 開機自動啟動 |
| `auto_connect` | `false` | 啟動後自動連線 |
| `local_proxy_enabled` | `true` | 執行本機 SOCKS5 監聽 |
| `local_socks5_listen` | `"127.0.0.1:1080"` | 監聽位址（僅回送） |
| `desktop_network_mode` | `"socks5_only"` | 或 `"lan_routes_tun"` 啟用原生路由 |
| `lan_routes` | `[]` | `lan_routes_tun` 模式下要安裝的路由 |
| `tunnel_first` | `false` | 讓 Tunnel 路由壓過重疊的本地內網路由 |
| `exported_lans` | `[]` | 本 Peer 對外發布的私有網段 |
| `auto_export_current_lan` | `true` | 同時發布本機目前接上的網段 |
| `client_access` | 開放 | 存取 ACL —— 見[上文](#決定誰能連到你) |
| `p2p_allow_lan_candidates` | `false` | 把內網位址作為直連候選提供出去 |
| `log_level` | `"info"` | Client 日誌等級 |

未知的鍵會被拒絕而不是忽略，所以拼錯了會報錯，不會靜默失效。

---

## 檔案放在哪

| 是什麼 | 路徑 |
|---|---|
| Client 設定、已匯入的設定檔、密鑰 | `~/.lantunnel/app/`（可用 `TUNNEL_PROXY_APP_CONFIG_DIR` 覆寫） |
| Client 設定檔 | `~/.lantunnel/app/settings.json` |
| Gateway 設定 | `configs/gateway.yaml`（或 `--config` 指定） |
| Gateway 的 Tunnel 放行檔案 | `state/scopes.d/*.scope` |
| Gateway 中繼用量帳本 | `state/relay-usage.wal` |
| Tunnel owner 檔案 | `init-tunnel --output-dir` 指定的位置 —— 記得備份 |

匯入的私鑰存放在 Client 建立的僅擁有者可讀檔案裡；它不會寫進日誌、不會傳給 Gateway，也不會離開這台機器。

---

## 疑難排解

**Client 連不上。**
先確認 Gateway 正在執行，且它的資料連接埠從外部連得到（`nc -z gw.example.com 8443`，QUIC 用 `nc -zu`）。接著確認這條 Tunnel 的 `.scope` 在 Gateway 的 `scopes_dir` 裡 —— 沒有它，Gateway 沒有任何理由放你進來。

**出現「Peer already attached」，或某個 Client 一直被踢掉。**
兩個 Client 在用同一份 `.peer`。用 `add-peer` 再簽一份；設定檔是一台裝置的身分，不是共用憑證。

**連得通，但一直走中繼。**
看介面上的流量計數器 —— 它把直連與中繼分開統計。兩端都是對稱式 NAT 時打洞會失敗。如果兩個 Peer 在同一個內網裡，加上 `--enable-lan-p2p` 讓本地位址也成為候選。另外確認設定的 UDP 對應連接埠（預設 `8444`）能到達 Gateway；沒有對應探測，兩邊都學不到自己的公網對應。

**直連可以但中繼不行（或反過來）。**
這是兩條彼此獨立的路徑。中繼依賴 Gateway 的資料連接埠；直連依賴 UDP 能在兩個 Peer 之間打通。一次只測一條。

**遠端服務拒絕連線。**
是目標 Client 的存取政策拒絕的 —— 去檢查**那台機器**上的 `client_access`，不是你這台。`NotAuthorized` 是最終結果，不會再去嘗試其他 Peer。

**匯出的內網連不到。**
匯出方的 Client 必須目前確實接在那個網段上，匯出才處於就緒狀態 —— 設定的網段只有與已連線的網段完全一致時才會被發布。確認之後，再檢查那台 Client 的存取政策是否放行了具體的目標與連接埠。

**版本不相符。**
Peer、Gateway 與設定檔必須在同一條 2.0.x 線上。線格式不做跨版本協商，混版本部署會直接失敗。

**想看更多細節。**
Client 用 `--log-level debug`，Gateway 設定裡設 `log.level: debug`。日誌裡不會出現私鑰、設定檔內容或工作階段金鑰。

---

## 接下來看什麼

- **[lantunnel.app](https://lantunnel.app/)** —— 免費 Tunnel、託管 Gateway、下載，以及遊戲串流、私有 AI 工具、家庭服務的專題指南。
- **[CONTEXT.md](../CONTEXT.md)** —— 各部分如何組合，以及每個術語的確切意義。
- **[PROTOCOL.md](./PROTOCOL.md)** —— 線格式規範，需要對接實作時看這個。
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** —— 建置、測試、送出修補。

---

> 本文是英文 [USAGE.md](./USAGE.md) 的繁體中文版。兩者出現分歧時，以英文版為準。
