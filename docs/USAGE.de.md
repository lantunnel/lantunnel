# Lantunnel verwenden

Eine praktische Anleitung: verbinden, deine Maschinen erreichen und selbst bestimmen, wer dich erreicht.

Neu im Projekt? Fang mit der [README](../README.de.md) an. Interessiert dich der Entwurf dahinter? [CONTEXT.md](../CONTEXT.md).

[English](./USAGE.md) ·
[简体中文](./USAGE.zh-CN.md) ·
[繁體中文](./USAGE.zh-TW.md) ·
[日本語](./USAGE.ja.md) ·
[Español](./USAGE.es.md) ·
**Deutsch** ·
[Français](./USAGE.fr.md)

**Inhalt**

1. [Die Idee in einer Minute](#die-idee-in-einer-minute)
2. [Welchen Weg nehmen?](#welchen-weg-nehmen)
3. [Weg A — gehostetes Gateway (am schnellsten)](#weg-a--gehostetes-gateway-am-schnellsten)
4. [Weg B — eigenes Gateway](#weg-b--eigenes-gateway)
5. [Dinge erreichen](#dinge-erreichen)
6. [Ein ganzes LAN freigeben](#ein-ganzes-lan-freigeben)
7. [Bestimmen, wer dich erreicht](#bestimmen-wer-dich-erreicht)
8. [Auf einem Server (headless)](#auf-einem-server-headless)
9. [Smartphones](#smartphones)
10. [Befehlsreferenz](#befehlsreferenz)
11. [Einstellungsreferenz](#einstellungsreferenz)
12. [Wo die Dateien liegen](#wo-die-dateien-liegen)
13. [Fehlersuche](#fehlersuche)

---

## Die Idee in einer Minute

Ein **Tunnel** ist ein kleines privates Netz aus Maschinen, die einander vertrauen. Jede Maschine darin ist ein **Peer**, und jeder Peer besitzt genau ein signiertes **`.peer`-Profil** — seine Identität, seinen privaten Schlüssel und den Weg zum Gateway.

Sobald zwei Peers im selben Tunnel sind, sprechen sie direkt miteinander, wann immer das Netz es zulässt. Wenn nicht, weichen sie auf ein Relay über das **Gateway** aus, das versiegelte Bytes weiterreicht, die es nicht lesen kann. Das Gateway ist ein Treffpunkt, kein Mittelsmann.

Drei Dinge brauchst du nie: eine öffentliche IP in deinem LAN, einen weitergeleiteten Router-Port oder ein gemeinsames Passwort.

## Welchen Weg nehmen?

|  | **Gehostetes Gateway** | **Eigenes Gateway** |
|---|---|---|
| Du betreibst | nur den Client | Client und dein eigenes Gateway |
| Du brauchst | ein Konto bei [lantunnel.app](https://lantunnel.app/) | eine Maschine mit öffentlicher Adresse |
| Einrichtungszeit | Minuten | etwa 20 Minuten |
| Relay | 5 GB im Monat gratis, darüber gemessen | dein eigenes, ungemessen |
| Direktes P2P | unbegrenzt | unbegrenzt |

Beide Wege nutzen denselben Client und dasselbe Protokoll. Du kannst gehostet anfangen und später umziehen — oder beides parallel betreiben, denn ein Tunnel ist von jedem Konto unabhängig.

---

## Weg A — gehostetes Gateway (am schnellsten)

**[lantunnel.app](https://lantunnel.app/)** betreibt die Gateway-Flotte für dich. Jedes Konto bekommt einen dauerhaft kostenlosen Tunnel: unbegrenzten Direktverkehr, beliebig viele LAN-Geräte hinter jedem Client und 5 GB verschlüsseltes Relay pro Monat für die Fälle, in denen die Direktverbindung scheitert.

1. **Tunnel anlegen** — bei [lantunnel.app](https://lantunnel.app/) registrieren und den kostenlosen Tunnel erstellen. Keine Gateway-Adresse, kein Zertifikat, kein DNS.
2. **Pro Gerät einen Peer hinzufügen** — einen fürs Notebook, einen fürs NAS, einen für den Desktop. Lade jedes `.peer`-Profil herunter.
3. **Client installieren** — von [lantunnel.app/download](https://lantunnel.app/download) oder aus diesem Repository gebaut.
4. **Importieren und verbinden:**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # öffnet die Oberfläche, dort verbinden
   ```

Ein verwaltetes Profil enthält nur die Platform-URL. Beim Verbinden fragt der Client, auf welchem Gateway sein Tunnel gerade liegt, signiert die Anfrage mit seinem eigenen Schlüssel und bekommt die Verbindungsdaten zurück. Wechselt das Gateway, muss auf deinen Geräten nichts angepasst werden.

Du kannst direkt zu [Dinge erreichen](#dinge-erreichen) springen.

---

## Weg B — eigenes Gateway

Alles Folgende liegt unter Apache-2.0 in diesem Repository. Nichts davon nimmt Kontakt zu lantunnel.app auf.

### Was du brauchst

- Eine aus dem Internet erreichbare Maschine — ein VPS für 5 Dollar reicht völlig. Das Gateway macht überwiegend Signalisierung, und das Relay trägt nur das, was direkt nicht geht.
- Zwei eingehende Regeln darauf: deinen **Datenport** (TCP oder UDP, je nach Transport) und den **UDP-Mapping-Port** (Vorgabe `8444`).
- Ein TLS-Zertifikat. Ein echtes oder ein selbst signiertes, das du anpinnst — beides funktioniert.

### 1. Binaries bauen (oder herunterladen)

```bash
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin
```

### 2. Dem Gateway ein Zertifikat geben

Ein echtes Zertifikat für deinen Hostnamen funktioniert unverändert. Für ein selbst signiertes:

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

Ohne Hostnamen nimmst du statt eines DNS-SAN ein **IP-SAN** (`IP:203.0.113.10`).

### 3. Den Tunnel offline erzeugen

`lantunnel-admin` spricht nie mit dem Netz. Führe es aus, wo du magst; die erzeugte `.tunnel`-Datei ist der Signaturschlüssel des Tunnels und gehört an einen sicheren Ort.

```bash
mkdir -p provision
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443 \
  --gateway-cert certs/server.crt \
  --output-dir ./provision
```

Das schreibt zwei Dateien, benannt nach der erzeugten Tunnel-ID:

| Datei | Für wen | Inhalt |
|---|---|---|
| `<tunnel-id>.tunnel` | **Nur für dich.** Modus `0600`. | Der private Signaturschlüssel des Tunnels. Verlierst du ihn, kannst du keine weiteren Peers ausstellen; gerät er nach außen, kann es jemand anderes. |
| `<tunnel-id>.scope` | Fürs Gateway. Öffentlich. | Tunnel-ID und der *öffentliche* Signaturschlüssel — mehr nicht. Damit lassen sich weder Peers ausstellen noch Daten mitlesen. |

Optionen von `init-tunnel`:

- `--gateway-transport quic | websocket | grpc` — QUIC ist die Standardwahl und die einzige Variante mit eigenem Stream pro Flow. WebSocket und gRPC sind für Netze gedacht, die UDP blockieren.
- `--gateway-host` und/oder `--gateway-ip` — mit beiden wird die IP angewählt und der Hostname als TLS-Servername verwendet.
- `--gateway-cert` — das anzupinnende PEM. Bei einem öffentlich vertrauenswürdigen Zertifikat weglassen.

### 4. Pro Gerät ein Profil ausstellen

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

Jeder `add-peer`-Aufruf vergibt eine **Overlay-IP** aus `198.18.0.0/16`, erzeugt ein frisches Schlüsselpaar, signiert die Mitgliedschaft und aktualisiert die Besitzerdatei atomar.

> **Ein `.peer` pro Gerät.** Ein Profil auf eine zweite Maschine zu kopieren klont keinen Peer — beide Instanzen streiten um dieselbe Identität, und das Gateway weist die unterlegene ab.

Nützliche Schalter: `--overlay-ip`, um eine Adresse festzulegen, und `--replicas`, um diesem Peer mehr als eine gleichzeitige Transportverbindung zu erlauben.

### 5. Das Gateway starten

Kopiere **nur den öffentlichen Scope** auf den Gateway-Host:

```bash
mkdir -p state/scopes.d
cp ./provision/<tunnel-id>.scope state/scopes.d/
```

Starte es mit einer Konfiguration auf Basis von [`configs/gateway.yaml`](../configs/gateway.yaml):

```bash
lantunnel-gateway --config configs/gateway.yaml
```

Worauf es ankommt:

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # muss zu --gateway-port passen
  transport_type: "quic"          # muss zu --gateway-transport passen
  tls_cert: "certs/server.crt"
  tls_key: "certs/server.key"
  scopes_dir: "state/scopes.d"    # hier landen die .scope-Dateien
  mapping_probe_port: 8444        # UDP; das Gateway bindet ihn selbst
```

Das Gateway bindet seinen Mapping-Socket selbst — es gibt keinen zweiten Prozess zu starten. Betreibst du mehrere Gateways auf einem Host, gib jedem seinen eigenen Datenport *und* seinen eigenen Mapping-Port. Ein QUIC-Datenlistener kann den Mapping-Port nicht mitbenutzen.

Einen weiteren Tunnel fügst du später hinzu, indem du eine weitere `.scope` in `scopes_dir` legst. Beispiel-Units für systemd liegen in [`scripts/remote/`](../scripts/remote/).

### 6. Die Geräte verbinden

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # zur Kontrolle; gibt nie private Schlüssel aus
lantunnel-client                      # Oberfläche
lantunnel-client connect <tunnel-id>  # oder headless
```

---

## Dinge erreichen

Sobald du verbunden bist, gibt es zwei Wege, Verkehr in den Tunnel zu schicken.

### 1. Der lokale SOCKS5-Proxy — immer aktiv

Jeder verbundene Client stellt einen SOCKS5-Proxy auf **`127.0.0.1:1080`** bereit, ausschließlich auf Loopback und ohne Authentifizierung. Er braucht keine: Er hängt am Loopback, und jede Anfrage darüber wird gegen die Richtlinie des *Ziel*-Peers geprüft.

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # Jellyfin auf einem Peer
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # NAS im LAN eines Peers
```

Browser, `ssh -o ProxyCommand`, Docker und die meisten Kommandozeilenwerkzeuge nehmen einen SOCKS5-Proxy direkt entgegen. Ist `1080` belegt, verschiebst du den Listener mit `--local-socks5-listen 127.0.0.1:1081`.

Bei verbundenem Client kopiert das Einstellungsfenster auf dem Desktop ein fertiges Clash-YAML-Snippet für diesen Listener.

### 2. Natives Routing — jede Anwendung, ohne Konfiguration

Mit aktiviertem nativem Routing legt die Maschine echte Routen für den Tunnel an, sodass *jede* Anwendung Peers über ihre Adresse erreicht, ohne von Lantunnel zu wissen.

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

Alternativ stellst du den Netzwerkmodus in der Desktop-Oberfläche um und trägst die Routen dort ein. Auf dem Smartphone gibt es diese Wahl nicht — der VPN-Dienst ist der einzige Weg zum Verkehr anderer Apps, also gilt es dort immer.

**Tunnel First** entscheidet, was passiert, wenn eine entfernte Tunnel-Route sich mit dem Netz überschneidet, in dem du physisch steckst. Aus (Vorgabe) gewinnt dein lokales LAN, an gewinnt der Tunnel — praktisch, wenn das Café-WLAN ebenfalls `192.168.1.0/24` verwendet. Gateway, Steuerkanal, DNS und selbst exportierte Ziele bleiben in beiden Fällen auf ihren nativen Routen geschützt.

### Welche Adresse nehme ich?

| Um zu erreichen | Nimm |
|---|---|
| Einen Dienst auf der entfernten Peer-Maschine selbst | Deren **Overlay-IP** (`198.18.x.y`) mit dem Port des Dienstes. `lantunnel-client tunnel list` gibt sie als JSON aus, die Oberfläche zeigt sie ebenfalls. |
| Ein Gerät im LAN des entfernten Peers | Die **echte LAN-Adresse** dieses Geräts, etwa `192.168.1.50`. |

Ein Overlay-Port wird standardmäßig auf `127.0.0.1` mit demselben Port auf der Zielmaschine abgebildet.

---

## Ein ganzes LAN freigeben

Ein Peer kann die privaten Subnetze bekanntgeben, in denen er steht. Andere Peers erreichen dann über ihn *alles* in diesen Subnetzen — das NAS, den Drucker, die Weboberfläche des Switches — ohne dass auf diesen Geräten irgendetwas installiert wird.

Zwei voneinander unabhängige Quellen, beide in der Oberfläche standardmäßig aktiv:

- **Aktuelles LAN exportieren** (`auto_export_current_lan`, standardmäßig an) veröffentlicht die privaten Netze, mit denen diese Maschine gerade verbunden ist, und leitet sie bei jedem Interface-Scan neu ab. Trägst du das Notebook von zu Hause ins Büro, wandert der Export mit.
- **Selbst eingetragene Exporte** (`exported_lans`) sind Präfixe, die du benennst.

Schaltest du den Automatikschalter aus, wird nur zurückgenommen, was er hinzugefügt hat; deine eigene Liste bleibt unberührt.

Akzeptiert werden ausschließlich IPv4-Präfixe nach RFC1918. Standardrouten, öffentliche Bereiche, Loopback, Link-Local, Multicast und alles, was sich mit dem Overlay-Pool überschneidet, werden abgelehnt.

**Ein Export schafft Erreichbarkeit, keine Erlaubnis.** Ein entfernter Peer muss für jedes Ziel weiterhin die [Zugriffsrichtlinie](#bestimmen-wer-dich-erreicht) des exportierenden Clients passieren.

Exportieren zwei Peers dasselbe Präfix, wählt jeder Client den zuerst gesehenen und wechselt erst zum nächsten, wenn dessen letzter Pfad wegbricht. Das ist eine Entscheidung pro Client und wird nicht gespeichert; dass zwei deiner Maschinen unterschiedliche Exporteure wählen, ist also normal.

---

## Bestimmen, wer dich erreicht

Die **Client-Zugriffsrichtlinie** ist die einzige ACL in Lantunnel, und sie liegt auf der Maschine, die erreicht wird. Nicht auf dem Gateway. Nicht auf einem Server. Die Routenwahl entscheidet, *wohin* geschickt wird; dein Client entscheidet unabhängig davon, ob er bedient.

Vorgabe: Eine leere Richtlinie bedeutet, dass **jeder Peer mit einem Profil für deinen Tunnel dich erreichen darf**. Ein solches Profil zu bekommen setzte ohnehin voraus, dass du es ausgestellt hast; eine zweite Schranke darüber schuf keine neue Grenze, sondern machte frische Installationen nur unerklärlich unerreichbar. Sobald du eine Allow-Regel formulierst, wird sie zum einzigen Weg hinein. **Deny wird immer zuerst geprüft und gewinnt immer.**

Setze sie in der Desktop-Oberfläche oder in `settings.json`:

```jsonc
{
  "client_access": {
    "allow": [
      // SSH auf diese Maschine
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // Jellyfin auf dem NAS daneben
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // Alles im IoT-Subnetz, beliebiger TCP-Port
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // ... und der Router nie, egal was die Allow-Liste sagt
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

Ziele sind `this_peer`, `ip`, `cidr` oder `host`. Ports sind `any` oder `exact` — Portbereiche gibt es nicht. Die Reihenfolge der Regeln ist bedeutungslos; entscheidend ist allein, dass Deny über Allow steht. Eine Regel benennt nie einen Quell-Peer: Jedes authentifizierte Mitglied des Tunnels bekommt dieselbe Antwort.

Um alles abzulehnen, verweigerst du `0.0.0.0/0` und `::/0` für TCP und UDP. Genau das schreibt auch die Schaltfläche „gesamten eingehenden Verkehr blockieren" der Oberfläche, sodass die gespeicherte Datei dem entspricht, was du wolltest.

---

## Auf einem Server (headless)

`--headless` (Alias `--no-ui`) führt dieselbe Laufzeit ohne Fenster, Tray-Symbol und WebView aus — dieselbe Reconnect-Logik, dasselbe Verhalten von PeerLink und Relay, dieselben SOCKS5- und TUN-Schnittstellen.

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # im Vordergrund, ohne Oberfläche
lantunnel-client status --json                # aus einer zweiten Shell
lantunnel-client disconnect
```

Ein blankes `--headless` verbindet das für Autoconnect markierte Profil, sodass eine Service-Unit ohne Tunnel-ID auskommt:

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

Im Headless-Betrieb gibt es keine Einstellungsoberfläche; bearbeite `settings.json` im Konfigurationsverzeichnis direkt — siehe [Einstellungsreferenz](#einstellungsreferenz).

**Unter Windows** verwenden Release-Builds das GUI-Subsystem. Ein normaler Start öffnet daher kein Konsolenfenster, und `cmd.exe` wartet nicht auf den Prozess. Wenn Ausgabe und Exit-Status eines kurzen Befehls zählen, nimm `start /wait`:

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## Smartphones

Android (`apps/android-proxy`, VpnService) und iOS (`apps/ios-proxy`, NetworkExtension) führen über `tp-mobile-ffi` denselben Rust-Kern aus. Importiere das `.peer`-Profil, indem du seinen QR-Code scannst oder die Datei öffnest, und starte dann das VPN.

Auf dem Smartphone gibt es keinen Umschalter für den Netzwerkmodus: Der VPN-Dienst ist der einzige Weg zum Verkehr anderer Apps, also folgt natives Routing dort immer der Laufzeit.

---

## Befehlsreferenz

### `lantunnel-client`

```
lantunnel-client                          Desktop-Oberfläche öffnen
lantunnel-client connect <TUNNEL_ID>      Ein importiertes Profil ohne Oberfläche verbinden
lantunnel-client disconnect               Laufenden Client trennen
lantunnel-client status --json            Status als JSON ausgeben
lantunnel-client tunnel import <FILE>     Ein .peer-Profil importieren
lantunnel-client tunnel list              Profile als JSON auflisten
```

`tunnel list` gibt zu jedem importierten Profil Tunnel-ID, Peer-ID, Overlay-IP und Bootstrap-Art aus. Privates Schlüsselmaterial ist nicht serialisierbar und taucht nie auf.

| Option | Bedeutung |
|---|---|
| `--headless`, `--no-ui` | Vollständige Laufzeit ohne Oberfläche |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace` |
| `--local-socks5-listen <ADDR>` | Loopback-SOCKS5-Listener verlegen |
| `--desktop-network-mode <MODE>` | `socks5_only` oder `lan_routes_tun` |
| `--lan-route <CIDR>` | Eine native LAN-Route anlegen (wiederholbar) |
| `--enable-lan-p2p` | LAN-Adressen als Kandidaten für den Direktweg zulassen |
| `-V`, `--help` | Version, Hilfe |

Überschreibung per Umgebung: `LANTUNNEL_LOCAL_SOCKS5_LISTEN`, `LANTUNNEL_DESKTOP_NETWORK_MODE`, `LANTUNNEL_LAN_ROUTES`, `TUNNEL_PROXY_APP_CONFIG_DIR`.

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

Bewusst offline. Es weist symbolische Links ab und überschreibt keine vorhandene Datei.

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>]              Gateway ausführen
lantunnel-gateway onboard --pairing <FILE>       Ein Platform-verwaltetes Gateway anmelden
lantunnel-gateway mapping serve                  Eigenständiger UDP-Mapping-Reflektor
```

`--config` verwendet standardmäßig `configs/gateway.yaml`. `mapping serve` existiert für ungewöhnliche Aufbauten; ein normales Gateway bindet seinen Mapping-Socket selbst und braucht es nicht.

---

## Einstellungsreferenz

`settings.json` im Konfigurationsverzeichnis des Clients. Jeder Schlüssel ist optional.

| Schlüssel | Vorgabe | Bedeutung |
|---|---|---|
| `auto_start` | `false` | Beim Anmelden starten |
| `auto_connect` | `false` | Beim Start verbinden |
| `local_proxy_enabled` | `true` | Lokalen SOCKS5-Listener betreiben |
| `local_socks5_listen` | `"127.0.0.1:1080"` | Dessen Adresse (nur Loopback) |
| `desktop_network_mode` | `"socks5_only"` | Oder `"lan_routes_tun"` für native Routen |
| `lan_routes` | `[]` | Native Routen im Modus `lan_routes_tun` |
| `tunnel_first` | `false` | Tunnel-Routen gewinnen gegen überlappende lokale LAN-Routen |
| `exported_lans` | `[]` | Private Präfixe, die dieser Peer veröffentlicht |
| `auto_export_current_lan` | `true` | Auch die Netze veröffentlichen, in denen diese Maschine steht |
| `client_access` | offen | Die ACL — siehe [oben](#bestimmen-wer-dich-erreicht) |
| `p2p_allow_lan_candidates` | `false` | LAN-Adressen als Kandidaten für den Direktweg anbieten |
| `log_level` | `"info"` | Log-Level des Clients |

Unbekannte Schlüssel werden abgelehnt statt ignoriert, damit ein Tippfehler auffällt, anstatt still wirkungslos zu bleiben.

---

## Wo die Dateien liegen

| Was | Pfad |
|---|---|
| Client-Konfiguration, importierte Profile, Geheimnisse | `~/.lantunnel/app/` (überschreibbar mit `TUNNEL_PROXY_APP_CONFIG_DIR`) |
| Client-Einstellungen | `~/.lantunnel/app/settings.json` |
| Gateway-Konfiguration | `configs/gateway.yaml` (oder per `--config`) |
| Tunnel-Zulassung im Gateway | `state/scopes.d/*.scope` |
| Relay-Nutzungsjournal des Gateways | `state/relay-usage.wal` |
| Besitzerdatei des Tunnels | Wohin `init-tunnel --output-dir` sie gelegt hat — sichere sie |

Der importierte private Schlüssel liegt in einer Datei, die der Client nur für den Eigentümer lesbar anlegt. Er wird nie in ein Log geschrieben, nie an ein Gateway geschickt und verlässt die Maschine nie.

---

## Fehlersuche

**Der Client verbindet sich nicht.**
Prüfe, ob das Gateway läuft und sein Datenport von außen erreichbar ist (`nc -z gw.example.com 8443`, für QUIC `nc -zu`). Vergewissere dich dann, dass die `.scope` des Tunnels im `scopes_dir` des Gateways liegt — ohne sie hat das Gateway keinen Grund, dich zuzulassen.

**„Peer already attached" — oder ein Client fliegt ständig raus.**
Zwei Clients laufen mit demselben `.peer`. Stelle mit `add-peer` ein zweites Profil aus; ein Profil ist die Identität eines Geräts, kein geteiltes Zugangsdatum.

**Alles funktioniert, aber immer über Relay.**
Sieh dir die Verkehrszähler in der Oberfläche an — sie trennen Direkt von Relay. Symmetrisches NAT auf beiden Seiten kann Hole-Punching vereiteln. Sind beide Peers im selben LAN, ergänze `--enable-lan-p2p`, damit lokale Adressen als Kandidaten angeboten werden. Prüfe außerdem, ob UDP `8444` das Gateway erreicht; ohne die Mapping-Probe erfährt keiner der beiden Peers sein öffentliches Mapping.

**Direkt geht, Relay nicht (oder umgekehrt).**
Das sind unabhängige Pfade. Relay braucht den Datenport des Gateways, direkt braucht UDP zwischen den Peers. Teste eins nach dem anderen.

**Ein entfernter Dienst lehnt die Verbindung ab.**
Es ist die Zugriffsrichtlinie des Ziel-Clients, die ablehnt — prüfe `client_access` *auf jener Maschine*, nicht auf deiner. Ein `NotAuthorized` ist endgültig und weicht nie auf einen anderen Peer aus.

**Ein exportiertes LAN ist nicht erreichbar.**
Der exportierende Client muss gerade tatsächlich mit diesem Netz verbunden sein, damit der Export bereit ist — ein konfiguriertes Präfix wird nur veröffentlicht, wenn es exakt einem verbundenen entspricht. Prüfe danach, ob die Zugriffsrichtlinie jenes Clients das konkrete Ziel und den Port erlaubt.

**Versionen passen nicht zusammen.**
Peers, Gateways und Profile müssen auf derselben 2.0.x-Reihe liegen. Das Wire-Format wird nicht zwischen Versionen ausgehandelt; gemischte Installationen scheitern geschlossen.

**Mehr Details bekommen.**
`--log-level debug` beim Client, `log.level: debug` in der Gateway-Konfiguration. Logs enthalten nie private Schlüssel, Profilinhalte oder Sitzungsschlüssel.

---

## Wie es weitergeht

- **[lantunnel.app](https://lantunnel.app/)** — kostenloser Tunnel, verwaltete Gateways, Downloads sowie Anleitungen für Game-Streaming, private KI-Werkzeuge und Dienste zu Hause.
- **[CONTEXT.md](../CONTEXT.md)** — wie die Teile zusammenpassen und was jeder Begriff genau bedeutet.
- **[PROTOCOL.md](./PROTOCOL.md)** — das Wire-Format, falls du dagegen implementierst.
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** — bauen, testen und einen Patch einreichen.

---

> Dies ist die deutsche Fassung von [USAGE.md](./USAGE.md). Bei Abweichungen gilt die englische Fassung.
