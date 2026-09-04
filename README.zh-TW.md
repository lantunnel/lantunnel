<h1 align="center">Lantunnel</h1>

<p align="center">
  <strong>把你的私有網路隨身帶著走。</strong><br>
  在任何地方連回自己內網裡的機器與服務 —— 優先點對點直連，端到端加密，
  不必做連接埠轉發，也不必把任何東西掛上公網。
</p>

<p align="center">
  <a href="https://github.com/lantunnel/lantunnel/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/lantunnel/lantunnel/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://lantunnel.app/"><img alt="Website" src="https://img.shields.io/badge/website-lantunnel.app-2563eb"></a>
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue"></a>
  <img alt="Rust" src="https://img.shields.io/badge/rust-1.89%2B-orange">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Windows%20%7C%20Linux%20%7C%20Android%20%7C%20iOS-lightgrey">
</p>

<p align="center">
  <a href="https://lantunnel.app/">官方網站</a> ·
  <a href="https://lantunnel.app/download">下載</a> ·
  <a href="./docs/USAGE.zh-TW.md">使用指南</a> ·
  <a href="./CONTEXT.md">架構文件</a> ·
  <a href="./docs/PROTOCOL.md">協定規範</a>
</p>

<p align="center">
  <a href="./README.md">English</a> ·
  <a href="./README.zh-CN.md">简体中文</a> ·
  <b>繁體中文</b> ·
  <a href="./README.ja.md">日本語</a> ·
  <a href="./README.es.md">Español</a> ·
  <a href="./README.de.md">Deutsch</a> ·
  <a href="./README.fr.md">Français</a>
</p>

---

NAS 放在家裡。跑模型的機器在辦公室。`ollama` 裝在你出門時留在桌上的桌機。它們全都躲在 NAT 後面，而且沒有一台該暴露在公開網際網路上。

Lantunnel 把這些機器組成一個小型私有網路 —— 一條 **Tunnel** —— 只有拿到你簽發設定檔的人才進得來。網路條件允許時，節點之間**直連**；直連打不通，就退回經由 Gateway 的**加密中繼**，而 Gateway 轉送的是連它自己都解不開的密文。兩條路徑都一樣：不必對外發布任何東西，路由器上不必開任何連接埠，中途也沒有任何一段看得到明文。

> ### 🚀 不想自己架 Gateway？那就別架。
>
> **[lantunnel.app](https://lantunnel.app/)** 為每個帳號提供一條**永久免費的 Tunnel** —— 點對點流量不限量，每個 Client 後面接多少台內網裝置都不限，另外每月 5 GB 加密中繼額度，留給直連打不通的時候。建立 Tunnel、下載 Client、匯入設定檔，就這樣。不必準備伺服器、憑證或 DNS。
>
> 想自己託管 Gateway 也沒問題 —— 完整程式碼就在這個儲存庫裡，Apache-2.0 授權，而且完全不計量。
>
> **[→ 領取你的免費 Tunnel](https://lantunnel.app/)**

---

## 你會得到什麼

| | |
|---|---|
| **直連優先** | 新連線會先嘗試點對點 QUIC 直連，搭配 UDP 打洞。中繼是備援，不是預設路徑。 |
| **端到端加密** | 中繼流量以 XChaCha20-Poly1305 封裝，金鑰來自兩個 Peer 之間的 X25519 協商。Gateway 轉送的是它解不開的位元組。 |
| **免連接埠轉發** | 所有 Peer 都主動向外撥號。你內網裡的任何東西都不需要輸入規則、公網 IP 或網域名稱。 |
| **整個內網都連得到** | 一個 Peer 可以發布自己所在的私有網段 —— 網路裡放一台 Client，NAS、印表機、內部儀表板就都能被 Tunnel 中的其他人存取。 |
| **存取控制在你手上** | 每個 Client 自行決定對外提供什麼。政策存放在被存取的那台機器上 —— 不在 Gateway，也不在任何伺服器。 |
| **一個程式，有無介面皆可** | `lantunnel-client` 預設開啟桌面視窗，加上 `--headless` 就是同一套執行環境，跑在伺服器上。 |
| **平台齊全** | macOS、Windows、Linux、Android、iOS。 |

### 大家實際拿它做什麼

- **遊戲與影音串流** —— 連回家中那台機器上的 Sunshine/Moonlight、Jellyfin、Plex。
- **私有 AI 與開發工具** —— Ollama、Open WebUI、內部 API、測試機、絕不能離開內網的資料庫。
- **家庭與辦公服務** —— NAS、Home Assistant、攝影機、內部儀表板、SSH。

## 運作方式

```
        ┌──────────────────── 一條 Tunnel ────────────────────┐
        │                                                      │
   ┌────┴─────┐                                          ┌─────┴────┐
   │  Peer A  │◀────────── QUIC 直連（優先） ───────────▶│  Peer B  │
   │  筆電    │                                          │ 家中 NAS │
   └────┬─────┘                                          └─────┬────┘
        │                                                      │
        │              ┌───────────────────────┐               │
        └─────────────▶│      Gateway          │◀──────────────┘
        加密中繼       │  會合點 +             │      加密中繼
        （備援）       │  NAT 穿透信令 +       │      （備援）
                       │  不透明轉送           │
                       └───────────────────────┘
                        它只看得到密文
```

整套系統就這三個部分：

- **`lantunnel-client`** 安裝在每台加入的裝置上。匯入一份簽章過的 `.peer` 設定檔，連上 Gateway，然後在本機開一個 SOCKS5 代理，也可以選擇安裝原生路由 —— 這樣一般應用程式不必知道 Lantunnel 存在就能連到 Tunnel。
- **`lantunnel-gateway`** 是會合點與 NAT 穿透的信令方。它靠一份公開的 `.scope` 檔案放行某條 Tunnel，協助兩個 Peer 打通直連，打不通就轉送封裝好的位元組。它不持有任何 Peer 私鑰，也看不到明文。
- **`lantunnel-admin`** 離線建立 Tunnel。兩個指令：`init-tunnel` 產生 owner 檔案與給 Gateway 用的公開 scope，`add-peer` 為每台裝置簽發一份設定檔。它完全不連網。

身分靠簽章，不靠共用密碼。沒有 Tunnel 密碼、沒有群組金鑰、也沒有 bearer token —— 每個 Peer 持有自己的 Ed25519 私鑰，每次接入都要證明自己擁有它，而這把私鑰永遠不離開產生它的那台機器。

📖 **[架構與概念 →](./CONTEXT.md)**  ·  📐 **[線格式規範 →](./docs/PROTOCOL.md)**

## 快速上手

### 最快的路 —— 使用託管 Gateway

1. 到 **[lantunnel.app](https://lantunnel.app/)** 建立一條免費 Tunnel。
2. 為每台裝置新增一個 Peer，下載對應的 `.peer` 設定檔。
3. 從 **[lantunnel.app/download](https://lantunnel.app/download)** 安裝 Client，匯入設定檔。

這樣就好。把應用程式的代理指向 `127.0.0.1:1080`，或開啟原生路由直接用內網位址連線。

### 自己來 —— 你的 Gateway，你的規則

```bash
# 1. 離線建立 Tunnel。這一步全程不連網。
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443
#   → <tunnel-id>.tunnel   這是 Tunnel 的簽章私鑰，務必妥善保管
#   → <tunnel-id>.scope    公開檔案，Gateway 只需要這個

# 2. 為每台裝置簽發一份設定檔。
lantunnel-admin add-peer --tunnel <tunnel-id>.tunnel --name laptop --output laptop.peer
lantunnel-admin add-peer --tunnel <tunnel-id>.tunnel --name nas    --output nas.peer

# 3. 把公開 scope 放到 Gateway 主機上，然後啟動。
mkdir -p state/scopes.d && cp <tunnel-id>.scope state/scopes.d/
lantunnel-gateway --config configs/gateway.yaml

# 4. 每台裝置匯入自己那份設定檔並連線。
lantunnel-client tunnel import ./laptop.peer
lantunnel-client                          # 桌面介面
lantunnel-client connect '<tunnel_id>'    # 同一套執行環境，不開視窗
```

一台裝置一份設定檔 —— `.peer` 不是拿來到處複製的。

📘 **[完整使用指南 —— 安裝、內網發布、存取規則、伺服器部署、手機端、疑難排解 →](./docs/USAGE.zh-TW.md)**

## 儲存庫內容

自行執行 Lantunnel 所需的一切，全部 Apache-2.0：

| 路徑 | 內容 |
|---|---|
| `apps/lantunnel-client` | Client。Tauri 桌面介面 + headless 執行環境，同一個執行檔。 |
| `apps/lantunnel-gateway` | Gateway。 |
| `apps/lantunnel-admin` | 離線簽發工具：`init-tunnel`、`add-peer`。 |
| `apps/android-proxy` | Android 應用程式（VpnService）。 |
| `apps/ios-proxy` | iOS 應用程式（NetworkExtension）。 |
| `crates/tp-*` | 共用實作 —— 協定、傳輸層、代理、P2P、Gateway 與 Client 引擎。 |
| `docs/PROTOCOL.md` | 線格式規範（規範性文件）。 |
| `CONTEXT.md` | 架構與術語。 |
| `docs/USAGE.zh-TW.md` | 怎麼用。 |

lantunnel.app 上的託管平台 —— 帳號、計費、託管 Gateway 叢集 —— 是獨立的閉源服務，**不在**這個儲存庫裡。這裡的程式碼不依賴它。自行託管的部署全程不會連線到它。

## 從原始碼建置

需要 Rust 1.89+、`protoc`（gRPC 傳輸用）、Node（建置 Client 前端）。

```bash
# Gateway 與簽發工具
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin

# Client（先建置前端）
npm --prefix apps/lantunnel-client/frontend ci
npm --prefix apps/lantunnel-client/frontend run build
cargo build --release -p lantunnel-client
```

Linux 上 Client 會連結 webkit2gtk、appindicator 與 rsvg；需要安裝哪些 `-dev` 套件見 [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)。

檢查項目，以及一套三 Peer 端到端驗收 —— 它會先走直連、再走加密中繼，把每個方向的 TCP 與 UDP 組合各驗一次：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e/v2_docker/run.sh
```

## 版本相容性

Peer、Gateway 與設定檔必須來自同一條 2.0.x 線 —— 線格式不做跨版本協商。從 1.x 升上來？舊的設定檔匯不進來，請用 `lantunnel-admin` 重新簽發。

## 參與貢獻

歡迎提出 issue 與 PR —— 建置、測試與程式風格見 [CONTRIBUTING.md](./CONTRIBUTING.md)。發現安全漏洞？請依 [SECURITY.md](./SECURITY.md) 走私密回報，不要開公開 issue。

## 授權

Apache License 2.0 —— 見 [LICENSE](./LICENSE) 與 [NOTICE](./NOTICE)。

---

> 本文是英文 [README.md](./README.md) 的繁體中文版。兩者出現分歧時，以英文版為準。

<p align="center">
  <strong>不想折騰？</strong> 一條永久免費 Tunnel，直連流量不限，託管 Gateway 隨時待命。<br>
  <a href="https://lantunnel.app/"><strong>前往 lantunnel.app 開始 →</strong></a>
</p>
