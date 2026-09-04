# Lantunnel の使い方

実践的なガイドです。接続し、自分のマシンに到達し、誰が自分に到達できるかを自分で決めるまで。

はじめての方は [README](../README.ja.md) から。設計の背景を知りたい方は [CONTEXT.md](../CONTEXT.md) へ。

[English](./USAGE.md) ·
[简体中文](./USAGE.zh-CN.md) ·
[繁體中文](./USAGE.zh-TW.md) ·
**日本語** ·
[Español](./USAGE.es.md) ·
[Deutsch](./USAGE.de.md) ·
[Français](./USAGE.fr.md)

**目次**

1. [1 分でわかる仕組み](#1-分でわかる仕組み)
2. [どちらの道を選ぶか](#どちらの道を選ぶか)
3. [ルート A — ホスト型 Gateway（最短）](#ルート-a--ホスト型-gateway最短)
4. [ルート B — 自前の Gateway](#ルート-b--自前の-gateway)
5. [目的地への到達方法](#目的地への到達方法)
6. [LAN 全体を共有する](#lan-全体を共有する)
7. [誰に到達を許すか](#誰に到達を許すか)
8. [サーバーで動かす（headless）](#サーバーで動かすheadless)
9. [スマートフォン](#スマートフォン)
10. [コマンドリファレンス](#コマンドリファレンス)
11. [設定リファレンス](#設定リファレンス)
12. [ファイルの置き場所](#ファイルの置き場所)
13. [トラブルシューティング](#トラブルシューティング)

---

## 1 分でわかる仕組み

**Tunnel** とは、互いに信頼し合うマシンの小さなプライベートネットワークです。その中の各マシンが **Peer** で、Peer はそれぞれ署名済みの **`.peer` プロファイル** を 1 つ持ちます。プロファイルには、そのデバイスの身元、秘密鍵、そして Gateway の見つけ方が入っています。

同じ Tunnel に属する 2 つの Peer は、ネットワークが許す限り直接やり取りします。許さない場合は **Gateway** 経由のリレーにフォールバックしますが、Gateway が転送するのは Gateway 自身にも読めない封をされたバイト列です。Gateway は待ち合わせ場所であって、間に立つ仲介者ではありません。

一方で、次の 3 つは決して必要ありません。LAN 側のグローバル IP、ルーターのポート転送、そして共有パスワードです。

## どちらの道を選ぶか

|  | **ホスト型 Gateway** | **自前の Gateway** |
|---|---|---|
| 動かすもの | Client だけ | Client と自分の Gateway |
| 必要なもの | [lantunnel.app](https://lantunnel.app/) のアカウント | グローバルアドレスを持つマシン |
| 所要時間 | 数分 | 20 分ほど |
| リレー | 月 5 GB まで無料、超過分は計測 | 自前なので計測なし |
| P2P 直結 | 無制限 | 無制限 |

どちらも同じ Client、同じプロトコルを使います。まずホスト型で始めて後から移行しても構いませんし、Tunnel はアカウントから独立しているので両方を並行して運用することもできます。

---

## ルート A — ホスト型 Gateway（最短）

**[lantunnel.app](https://lantunnel.app/)** が Gateway フリートを代わりに運用します。アカウントごとに恒久無料の Tunnel が 1 本付き、P2P 通信は無制限、各 Client の背後の LAN 機器数も無制限、加えて直結できなかったときのために月 5 GB の暗号化リレーが使えます。

1. **Tunnel を作る** — [lantunnel.app](https://lantunnel.app/) に登録して無料 Tunnel を作成します。Gateway のアドレスも証明書も DNS 設定も不要です。
2. **デバイスごとに Peer を追加** — ノート PC に 1 つ、NAS に 1 つ、デスクトップに 1 つ。それぞれの `.peer` プロファイルをダウンロードします。
3. **Client をインストール** — [lantunnel.app/download](https://lantunnel.app/download) から、またはこのリポジトリからビルドして。
4. **取り込んで接続：**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # UI が開くので、そこから接続
   ```

マネージドプロファイルに入っているのは Platform の URL だけです。接続時に Client が「この Tunnel は今どの Gateway にいるか」を問い合わせ、自分の鍵でリクエストに署名し、接続情報を受け取ります。Gateway が変わっても、デバイス側で書き換えるものは何もありません。

このまま[目的地への到達方法](#目的地への到達方法)へ進んで構いません。

---

## ルート B — 自前の Gateway

以下で使うものはすべてこのリポジトリの中にあり、Apache-2.0 です。lantunnel.app には一切接続しません。

### 用意するもの

- インターネットから到達できるマシン。月 5 ドルの VPS で十分です。Gateway の仕事はほとんどがシグナリングで、リレーが運ぶのは直結できなかった分だけです。
- そのマシンで受信を許可する 2 つの経路：**データポート**（トランスポートに応じて TCP または UDP）と **UDP マッピングポート**（既定 `8444`）。
- TLS 証明書。正規の証明書でも、自分でピン留めする自己署名証明書でも構いません。

### 1. バイナリをビルド（または入手）

```bash
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin
```

### 2. Gateway に証明書を用意する

ホスト名に対する正規の証明書はそのまま使えます。自己署名の場合は次のとおりです。

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

ホスト名がない場合は、DNS SAN の代わりに **IP SAN**（`IP:203.0.113.10`）を使ってください。

### 3. Tunnel をオフラインで作成する

`lantunnel-admin` はネットワークに一切アクセスしません。どのマシンで実行しても構いません。生成される `.tunnel` は、その Tunnel の署名鍵そのものなので、安全な場所に保管してください。

```bash
mkdir -p provision
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443 \
  --gateway-cert certs/server.crt \
  --output-dir ./provision
```

生成された Tunnel ID を名前に持つ 2 つのファイルが書き出されます。

| ファイル | 渡す相手 | 中身 |
|---|---|---|
| `<tunnel-id>.tunnel` | **自分だけ。** パーミッション `0600`。 | Tunnel の署名秘密鍵。失えば新しい Peer を発行できなくなり、漏れれば他人が発行できてしまいます。 |
| `<tunnel-id>.scope` | Gateway へ。公開ファイル。 | Tunnel ID と署名**公開**鍵のみ。Peer を発行することも、通信を読むこともできません。 |

`init-tunnel` のオプション：

- `--gateway-transport quic | websocket | grpc` — 既定は QUIC で、フローごとのストリームを持つ唯一の選択肢です。WebSocket と gRPC は UDP がブロックされる環境向けです。
- `--gateway-host` と `--gateway-ip`（片方または両方）— 両方を指定すると、接続先は IP、TLS サーバー名にはホスト名が使われます。
- `--gateway-cert` — ピン留めする PEM。公的に信頼された証明書を使う Gateway なら省略できます。

### 4. デバイスごとにプロファイルを発行する

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

`add-peer` は実行のたびに `198.18.0.0/16` から **Overlay IP** を割り当て、新しい鍵ペアを生成し、メンバーシップに署名し、オーナーファイルをアトミックに更新します。

> **1 デバイスにつき 1 つの `.peer`。** プロファイルを 2 台目にコピーしても Peer が増えるわけではありません。2 つのインスタンスが同じ身元を奪い合い、Gateway は後から来たほうを拒否します。

よく使うフラグ：`--overlay-ip` でアドレスを固定、`--replicas` でその Peer が同時に張れるトランスポート接続数を増やす。

### 5. Gateway を動かす

**公開 scope だけ** を Gateway のホストにコピーします。

```bash
mkdir -p state/scopes.d
cp ./provision/<tunnel-id>.scope state/scopes.d/
```

[`configs/gateway.yaml`](../configs/gateway.yaml) をもとにした設定で起動します。

```bash
lantunnel-gateway --config configs/gateway.yaml
```

重要な項目は次のとおりです。

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # --gateway-port と一致させること
  transport_type: "quic"          # --gateway-transport と一致させること
  tls_cert: "certs/server.crt"
  tls_key: "certs/server.key"
  scopes_dir: "state/scopes.d"    # .scope はここに置く
  mapping_probe_port: 8444        # UDP。Gateway 自身がバインドする
```

Gateway は自分でマッピング用ソケットをバインドします。別途起動するプロセスはありません。1 台のホストで複数の Gateway を動かす場合は、それぞれに専用のデータポート**と**専用のマッピングポートを与えてください。QUIC のデータリスナーがマッピングポートを共有することはできません。

あとから Tunnel を追加するときは、`scopes_dir` にもう 1 つ `.scope` を置くだけです。systemd のユニット例は [`scripts/remote/`](../scripts/remote/) にあります。

### 6. 各デバイスを接続する

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # 確認用。秘密鍵は出力されません
lantunnel-client                      # UI
lantunnel-client connect <tunnel-id>  # または headless で
```

---

## 目的地への到達方法

接続できたら、Tunnel にトラフィックを流す方法は 2 つあります。

### 1. ローカル SOCKS5 プロキシ — 常時有効

接続中の Client は必ず **`127.0.0.1:1080`** で SOCKS5 プロキシを公開します。ループバック限定、認証なしです。認証は必要ありません。ループバックにバインドされており、そこを通る各リクエストは**接続先**の Peer 自身のポリシーで判定されるからです。

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # ある Peer 上の Jellyfin
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # ある Peer の LAN にある NAS
```

ブラウザ、`ssh -o ProxyCommand`、Docker、そして大半の CLI ツールは SOCKS5 プロキシをそのまま指定できます。`1080` が使われている場合は `--local-socks5-listen 127.0.0.1:1081` で移してください。

Client が接続中であれば、デスクトップの設定画面からこのリスナー用の Clash YAML スニペットをそのままコピーできます。

### 2. ネイティブルーティング — すべてのアプリを、設定なしで

ネイティブルーティングを有効にすると、そのマシンは Tunnel 向けの実ルートを追加します。これにより **どの** アプリケーションも、Lantunnel の存在を知らないままアドレスで Peer に到達できます。

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

デスクトップ UI でネットワークモードを切り替えてルートを追加しても構いません。スマートフォンではこれは選択肢ではありません。他アプリのトラフィックを扱う手段が VPN サービスしかないため、常に適用されます。

**Tunnel First** は、リモートの Tunnel ルートが今いるネットワークと重なったときにどちらを優先するかを決めます。オフ（既定）ならローカル LAN が勝ち、オンなら Tunnel が勝ちます。カフェの Wi-Fi がたまたま `192.168.1.0/24` を使っているときに役立ちます。どちらの設定でも、Gateway、制御、DNS、自分自身のエクスポート宛はネイティブルートのまま保護されます。

### どのアドレスを使えばいい？

| 到達したい先 | 使うアドレス |
|---|---|
| リモート Peer のマシン自身で動くサービス | その **Overlay IP**（`198.18.x.y`）とサービスのポート。`lantunnel-client tunnel list` が JSON で出力し、UI にも表示されます。 |
| リモート Peer の LAN にあるデバイス | そのデバイスの**実際の LAN アドレス**（例：`192.168.1.50`）。 |

Overlay 上のポートは、既定では接続先マシンの `127.0.0.1` の同じポートにマッピングされます。

---

## LAN 全体を共有する

Peer は、自分が接続しているプライベートサブネットを広告できます。すると他の Peer はその Peer 経由でそれらのサブネット上の **あらゆる** 機器 — NAS、プリンター、スイッチの管理画面 — に到達できます。対象の機器側には何もインストールしません。

独立した 2 つの供給元があり、UI では既定でどちらも有効です。

- **現在の LAN をエクスポート**（`auto_export_current_lan`、既定で有効）は、このマシンが今つながっているプライベートネットワークを公開し、インターフェースを走査するたびに再導出します。ノート PC を自宅からオフィスへ持ち出せば、エクスポートもそれに追随します。
- **手入力のエクスポート**（`exported_lans`）は、自分で明示的に指定したプレフィックスです。

自動スイッチをオフにしても、取り下げられるのは自動で追加された分だけで、手入力の一覧はそのまま残ります。

受け付けるのは RFC1918 の IPv4 プレフィックスのみです。デフォルトルート、パブリックレンジ、ループバック、リンクローカル、マルチキャスト、および Overlay プールと重なるものはすべて拒否されます。

**エクスポートが生むのは到達性であって、許可ではありません。** リモートの Peer は、宛先ごとにエクスポート元 Client の[アクセスポリシー](#誰に到達を許すか)を通る必要があります。

2 つの Peer が同じプレフィックスをエクスポートしている場合、各 Client は最初に見つけたほうを選び、その最後の経路が失われたときに次へ切り替えます。これは Client ごとの判断で保存もされないため、手元の 2 台が別々のエクスポート元を選ぶのは正常な動作です。

---

## 誰に到達を許すか

**Client アクセスポリシー** は Lantunnel における唯一の ACL であり、到達される側のマシンに置かれます。Gateway でもサーバーでもありません。経路の選択は**どこへ送るか**を決め、あなたの Client が**応じるかどうか**を独立して決めます。

既定の挙動：ポリシーが空であることは、**あなたの Tunnel のプロファイルを持つすべての Peer が到達できる**という意味です。プロファイルを手にするには、そもそもあなたが発行する必要がありました。その上にもう一枚の関門を置いても新しい境界は生まれず、入れたての Client が理由もわからず到達不能になるだけでした。Allow ルールを 1 つでも書けば、それが唯一の入口になります。**Deny は常に先に評価され、常に優先されます。**

デスクトップ UI で設定するか、`settings.json` を直接編集します。

```jsonc
{
  "client_access": {
    "allow": [
      // このマシンへの SSH
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // 隣にある NAS 上の Jellyfin
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // IoT サブネットの任意の TCP ポート
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // …ただしルーターだけは Allow に何が書かれていても不可
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

宛先の種類は `this_peer`、`ip`、`cidr`、`host` です。ポートは `any` か `exact` で、範囲指定はサポートされません。ルールの順序に意味はなく、効くのは「Deny が Allow に勝つ」という一点だけです。ルールが送信元の Peer を指定することはありません。Tunnel の認証済みメンバーは全員同じ判定を受けます。

すべて拒否したい場合は、`0.0.0.0/0` と `::/0` を TCP と UDP の両方で Deny してください。UI の「すべての受信をブロック」が書き込むのもまさにこれなので、保存された内容と依頼した内容が一致します。

---

## サーバーで動かす（headless）

`--headless`（別名 `--no-ui`）は、ウィンドウもトレイも WebView も持たない、まったく同じランタイムを実行します。再接続ロジックも、PeerLink とリレーの挙動も、SOCKS5 と TUN も同一です。

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # フォアグラウンド、UI なし
lantunnel-client status --json                # 別のシェルから
lantunnel-client disconnect
```

`--headless` を単独で使うと自動接続に設定されたプロファイルにつながるため、サービスユニットに Tunnel ID を書く必要はありません。

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

headless モードに設定画面はないので、設定ディレクトリの `settings.json` を直接編集してください。[設定リファレンス](#設定リファレンス)を参照。

**Windows では**、リリースビルドが GUI サブシステムを使うため、通常起動でコンソールウィンドウは開かず、`cmd.exe` はプロセスの終了を待ちません。短いコマンドの出力と終了ステータスが必要な場合は `start /wait` を使ってください。

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## スマートフォン

Android（`apps/android-proxy`、VpnService）と iOS（`apps/ios-proxy`、NetworkExtension）は、`tp-mobile-ffi` を介して同じ Rust コアを実行します。`.peer` プロファイルは QR コードを読み取るかファイルを開いて取り込み、そのうえで VPN を開始してください。

スマートフォンにネットワークモードの切り替えはありません。他アプリのトラフィックに届く手段が VPN サービスだけなので、ネイティブルーティングは常にランタイムに従います。

---

## コマンドリファレンス

### `lantunnel-client`

```
lantunnel-client                          デスクトップ UI を開く
lantunnel-client connect <TUNNEL_ID>      取り込み済みプロファイルに UI なしで接続
lantunnel-client disconnect               動作中の Client を切断
lantunnel-client status --json            状態を JSON で出力
lantunnel-client tunnel import <FILE>     .peer プロファイルを 1 つ取り込む
lantunnel-client tunnel list              取り込み済みプロファイルを JSON で一覧表示
```

`tunnel list` は、取り込み済みの各プロファイルについて Tunnel ID、Peer ID、Overlay IP、ブートストラップ種別を出力します。秘密鍵はシリアライズ不可能で、決して現れません。

| オプション | 意味 |
|---|---|
| `--headless`、`--no-ui` | UI なしで完全なランタイムを実行 |
| `--log-level <LEVEL>` | `error`、`warn`、`info`、`debug`、`trace` |
| `--local-socks5-listen <ADDR>` | ループバック SOCKS5 リスナーの位置を変更 |
| `--desktop-network-mode <MODE>` | `socks5_only` または `lan_routes_tun` |
| `--lan-route <CIDR>` | ネイティブ LAN ルートを 1 つ追加（繰り返し可） |
| `--enable-lan-p2p` | LAN アドレスを直結候補として使うことを許可 |
| `-V`、`--help` | バージョン、ヘルプ |

環境変数による上書き：`LANTUNNEL_LOCAL_SOCKS5_LISTEN`、`LANTUNNEL_DESKTOP_NETWORK_MODE`、`LANTUNNEL_LAN_ROUTES`、`TUNNEL_PROXY_APP_CONFIG_DIR`。

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

設計上オフライン専用です。シンボリックリンクを拒否し、既存ファイルを上書きしません。

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>]              Gateway を実行
lantunnel-gateway onboard --pairing <FILE>       Platform 管理下の Gateway を登録
lantunnel-gateway mapping serve                  単体の UDP マッピングリフレクター
```

`--config` の既定値は `configs/gateway.yaml` です。`mapping serve` は特殊な構成のために用意されているもので、通常の Gateway は自分でマッピング用ソケットをバインドするため不要です。

---

## 設定リファレンス

Client の設定ディレクトリにある `settings.json`。すべてのキーは省略可能です。

| キー | 既定値 | 意味 |
|---|---|---|
| `auto_start` | `false` | ログイン時に起動 |
| `auto_connect` | `false` | 起動時に接続 |
| `local_proxy_enabled` | `true` | ローカル SOCKS5 リスナーを動かす |
| `local_socks5_listen` | `"127.0.0.1:1080"` | その待ち受けアドレス（ループバックのみ） |
| `desktop_network_mode` | `"socks5_only"` | ネイティブルートを使うなら `"lan_routes_tun"` |
| `lan_routes` | `[]` | `lan_routes_tun` モードで追加するルート |
| `tunnel_first` | `false` | 重なるローカル LAN ルートより Tunnel ルートを優先 |
| `exported_lans` | `[]` | この Peer が公開するプライベートプレフィックス |
| `auto_export_current_lan` | `true` | 現在つながっているネットワークも公開する |
| `client_access` | 開放 | ACL — [上記](#誰に到達を許すか)を参照 |
| `p2p_allow_lan_candidates` | `false` | LAN アドレスを直結候補として提示 |
| `log_level` | `"info"` | Client のログレベル |

未知のキーは無視されずに拒否されます。打ち間違いが黙って効かないままになることはありません。

---

## ファイルの置き場所

| 対象 | パス |
|---|---|
| Client 設定、取り込んだプロファイル、秘密情報 | `~/.lantunnel/app/`（`TUNNEL_PROXY_APP_CONFIG_DIR` で変更可） |
| Client の設定ファイル | `~/.lantunnel/app/settings.json` |
| Gateway の設定 | `configs/gateway.yaml`（または `--config` で指定） |
| Gateway の Tunnel 受け入れ | `state/scopes.d/*.scope` |
| Gateway のリレー使用量台帳 | `state/relay-usage.wal` |
| Tunnel オーナーファイル | `init-tunnel --output-dir` で指定した場所 — バックアップを |

取り込まれた秘密鍵は、Client が作成する所有者のみ読み取り可能なファイルに保存されます。ログに書かれることも、Gateway に送られることも、そのマシンから出ることもありません。

---

## トラブルシューティング

**Client が接続できない。**
まず Gateway が動いていること、そのデータポートに外部から到達できることを確認します（`nc -z gw.example.com 8443`、QUIC なら `nc -zu`）。次に、その Tunnel の `.scope` が Gateway の `scopes_dir` にあるかを確認してください。これがなければ、Gateway があなたを受け入れる理由がありません。

**「Peer already attached」が出る、または片方の Client が切られ続ける。**
2 つの Client が同じ `.peer` を使っています。`add-peer` で 2 つ目のプロファイルを発行してください。プロファイルは 1 台のデバイスの身元であって、共有する資格情報ではありません。

**つながるが、いつもリレー経由になる。**
UI のトラフィックカウンターを見てください。直結とリレーが分けて表示されます。両端が対称型 NAT だとホールパンチングは成立しません。2 つの Peer が同じ LAN にいるなら `--enable-lan-p2p` を付けてローカルアドレスを候補に加えます。UDP `8444` が Gateway に届いているかも確認してください。マッピングプローブがないと、どちらの Peer も自分の公開マッピングを知ることができません。

**直結は通るがリレーが通らない（あるいはその逆）。**
両者は独立した経路です。リレーには Gateway のデータポートが、直結には Peer 間で UDP が流れることが必要です。一度に片方ずつ切り分けてください。

**リモートのサービスが接続を拒否する。**
拒否しているのは接続先 Client のアクセスポリシーです。**そのマシン側**の `client_access` を確認してください。あなたの側ではありません。`NotAuthorized` は最終結果であり、別の Peer にフォールバックすることはありません。

**エクスポートした LAN に到達できない。**
エクスポート元の Client が現にそのネットワークに接続していなければ、エクスポートは準備完了になりません。設定したプレフィックスは、接続中のものと完全に一致したときだけ公開されます。それを確認したうえで、その Client のアクセスポリシーが該当の宛先とポートを許可しているかを見てください。

**バージョンの不一致。**
Peer、Gateway、プロファイルは同じ 2.0.x 系列である必要があります。ワイヤフォーマットはバージョン間でネゴシエートされず、混在した構成は接続を拒否して終わります。

**もっと詳しく調べたい。**
Client には `--log-level debug`、Gateway の設定には `log.level: debug` を指定します。ログに秘密鍵、プロファイルの内容、セッション鍵が含まれることはありません。

---

## 次に読むもの

- **[lantunnel.app](https://lantunnel.app/)** — 無料 Tunnel、マネージド Gateway、ダウンロード、そしてゲームストリーミング・プライベート AI・ホームサービス向けのガイド。
- **[CONTEXT.md](../CONTEXT.md)** — 各要素がどう噛み合うか、そして各用語の正確な意味。
- **[PROTOCOL.md](./PROTOCOL.md)** — 独自に実装するときのワイヤフォーマット。
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** — ビルド、テスト、パッチの送り方。

---

> 本書は英語版 [USAGE.md](./USAGE.md) の日本語訳です。内容に食い違いがある場合は英語版が優先します。
