# Using Lantunnel

A practical guide: get connected, reach your machines, and keep control of who reaches
yours.

New to the project? Start with the [README](../README.md). Want the design behind it?
[CONTEXT.md](../CONTEXT.md).

**Contents**

1. [The idea in one minute](#the-idea-in-one-minute)
2. [Pick a path](#pick-a-path)
3. [Path A — hosted Gateway (fastest)](#path-a--hosted-gateway-fastest)
4. [Path B — self-hosted Gateway](#path-b--self-hosted-gateway)
5. [Reaching things](#reaching-things)
6. [Sharing a whole LAN](#sharing-a-whole-lan)
7. [Deciding who reaches you](#deciding-who-reaches-you)
8. [Running on a server (headless)](#running-on-a-server-headless)
9. [Phones](#phones)
10. [Command reference](#command-reference)
11. [Settings reference](#settings-reference)
12. [Where files live](#where-files-live)
13. [Troubleshooting](#troubleshooting)

---

## The idea in one minute

A **Tunnel** is a small private network of machines that trust each other. Each machine in
it is a **Peer**, and each Peer holds one signed **`.peer` profile** — its identity, its
private key, and how to find the Gateway.

Once two Peers are in the same Tunnel, they talk directly whenever the network lets them.
When it doesn't, they fall back to a relay through the **Gateway**, which forwards sealed
bytes it cannot read. The Gateway is a meeting point, not a middleman.

Three things you never need: a public IP on your LAN, a forwarded router port, or a shared
password.

## Pick a path

|  | **Hosted Gateway** | **Self-hosted Gateway** |
|---|---|---|
| You run | Just the Client | Client + your own Gateway |
| You need | An account at [lantunnel.app](https://lantunnel.app/) | A machine with a public address |
| Setup time | Minutes | ~20 minutes |
| Relay | 5 GB/month free, metered above | Yours, unmetered |
| Direct P2P | Unlimited | Unlimited |

Both use the same Client and the same protocol. You can start hosted and move later — or
run both, since a Tunnel is independent of any account.

---

## Path A — hosted Gateway (fastest)

**[lantunnel.app](https://lantunnel.app/)** runs the Gateway fleet for you. Every account
gets one permanent Free Tunnel: unlimited direct peer-to-peer traffic, unlimited LAN
devices behind each Client, and 5 GB/month of encrypted relay for when direct fails.

1. **Create a Tunnel** — sign up at [lantunnel.app](https://lantunnel.app/) and create your
   Free Tunnel. No Gateway address, no certificate, no DNS to configure.
2. **Add a Peer per device** — one for the laptop, one for the NAS, one for the desktop.
   Download each `.peer` profile.
3. **Install the Client** — [lantunnel.app/download](https://lantunnel.app/download), or
   build it from this repository.
4. **Import and connect:**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # opens the UI, connect from there
   ```

A managed profile carries only the Platform URL. At connect time the Client asks which
Gateway its Tunnel is currently on, signs the request with its own key, and gets back the
connection facts. Change Gateways and nothing on your devices needs editing.

Skip to [Reaching things](#reaching-things).

---

## Path B — self-hosted Gateway

Everything below is in this repository under Apache-2.0. Nothing contacts lantunnel.app.

### What you need

- A machine reachable from the internet — a $5 VPS is plenty; the Gateway mostly does
  signaling, and relay only carries what direct paths can't.
- Two inbound rules on it: your **data port** (TCP or UDP depending on transport) and the
  **UDP mapping port** (default `8444`).
- A TLS certificate for it. A real one, or a self-signed one you pin — both work.

### 1. Build (or download) the binaries

```bash
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin
```

### 2. Give the Gateway a certificate

A real certificate for your hostname works as-is. For a self-signed one:

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout certs/server.key -out certs/server.crt \
  -subj "/CN=gw.example.com" \
  -addext "subjectAltName = DNS:gw.example.com" \
  -addext "basicConstraints = critical, CA:FALSE" \
  -addext "keyUsage = critical, digitalSignature, keyEncipherment" \
  -addext "extendedKeyUsage = serverAuth"
chmod 0600 certs/server.key
```

Use an **IP SAN** (`IP:203.0.113.10`) instead of a DNS SAN if you have no hostname.

### 3. Create the Tunnel — offline

`lantunnel-admin` never talks to the network. Run it wherever you like; the `.tunnel` file
it produces is the Tunnel's signing key and should stay somewhere safe.

```bash
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443 \
  --gateway-cert certs/server.crt \
  --output-dir ./provision
```

This writes two files named after the generated Tunnel ID:

| File | Who gets it | Contains |
|---|---|---|
| `<tunnel-id>.tunnel` | **Only you.** Mode `0600`. | The Tunnel signing private key. Lose it and you cannot issue more Peers; leak it and someone else can. |
| `<tunnel-id>.scope` | The Gateway. Public. | Tunnel ID and the signing *public* key — that's all. It cannot issue Peers or read traffic. |

Options for `init-tunnel`:

- `--gateway-transport quic | websocket | grpc` — QUIC is the default choice and the only
  one with per-flow streams. WebSocket and gRPC are for networks that block UDP.
- `--gateway-host` and/or `--gateway-ip` — with both, the IP is dialed and the host is used
  as the TLS server name.
- `--gateway-cert` — the PEM to pin. Omit it for a Gateway with a publicly trusted
  certificate.

### 4. Issue one profile per device

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

Each `add-peer` allocates an **Overlay IP** out of `198.18.0.0/16`, generates a fresh
keypair, signs the membership, and updates the owner file atomically.

> **One `.peer` per device.** Copying a profile onto a second machine does not clone a Peer
> — the two instances fight over the same identity and the Gateway rejects the loser.

Useful flags: `--overlay-ip` to pin an address, `--replicas` to allow more than one
simultaneous transport connection for that Peer.

### 5. Run the Gateway

Copy the **public scope only** to the Gateway host:

```bash
mkdir -p state/scopes.d
cp ./provision/<tunnel-id>.scope state/scopes.d/
```

Start it with a config based on [`configs/gateway.yaml`](../configs/gateway.yaml):

```bash
lantunnel-gateway --config configs/gateway.yaml
```

The settings that matter:

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # must match --gateway-port
  transport_type: "quic"          # must match --gateway-transport
  tls_cert: "certs/server.crt"
  tls_key: "certs/server.key"
  scopes_dir: "state/scopes.d"    # drop .scope files here
  mapping_probe_port: 8444        # UDP; the Gateway binds this itself
```

The Gateway binds its own mapping socket — there is no second process to start. If you run
several Gateways on one host, give each its own data port *and* its own mapping port. A
QUIC data listener cannot share the mapping port.

Adding a Tunnel later means dropping another `.scope` into `scopes_dir`. Example systemd
units are in [`scripts/remote/`](../scripts/remote/).

### 6. Connect the devices

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # confirm; never prints private keys
lantunnel-client                      # UI
lantunnel-client connect <tunnel-id>  # or headless
```

---

## Reaching things

Once connected you have two ways to send traffic into the Tunnel.

### 1. The local SOCKS5 proxy — always on

Every connected Client exposes a SOCKS5 proxy on **`127.0.0.1:1080`**, loopback only, no
authentication. It doesn't need one: it is bound to loopback and every request through it
is authorized against the *target* Peer's own policy.

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # Jellyfin on a Peer
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # NAS on a Peer's LAN
```

Browsers, `ssh -o ProxyCommand`, Docker, and most CLI tools take a SOCKS5 proxy directly.
Move the listener with `--local-socks5-listen 127.0.0.1:1081` if `1080` is taken.

When the Client is connected, the desktop settings panel will copy a ready-to-paste Clash
YAML snippet for this listener.

### 2. Native routing — every app, no configuration

Turn on native routing and the machine installs real routes for the Tunnel, so *any*
application reaches Peers by address without knowing Lantunnel exists.

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

Or in the desktop UI, switch the network mode and add the routes there. On a phone this
is not a choice — the VPN service is the only way to reach other apps' traffic, so it
always applies.

**Tunnel First** decides what happens when a remote Tunnel route overlaps a network you
are physically on. Off (the default), your local LAN wins. On, the Tunnel wins — useful
when you're on café Wi-Fi that happens to use `192.168.1.0/24` too. Gateway, control,
DNS, and self-export destinations stay on their native routes either way.

### Which address do I use?

| To reach | Use |
|---|---|
| A service on the remote Peer machine itself | Its **Overlay IP** (`198.18.x.y`) at the service's port. `lantunnel-client tunnel list` prints it as JSON, and the UI shows it. |
| A device on the remote Peer's LAN | That device's **real LAN address**, e.g. `192.168.1.50`. |

An Overlay port maps to `127.0.0.1` at the same port on the target machine by default.

---

## Sharing a whole LAN

A Peer can advertise the private subnets it sits on. Other Peers then reach *anything* on
those subnets through it — the NAS, the printer, the switch's web UI — without installing
anything on those devices.

Two independent sources, both on by default in the UI:

- **Export Current LAN** (`auto_export_current_lan`, on by default) publishes whatever
  private networks this machine is currently attached to, and re-derives them on every
  interface scan. Move the laptop from home to the office and the export follows it.
- **Typed exports** (`exported_lans`) are prefixes you name yourself.

Turning the automatic switch off withdraws only what it added; your typed list is
untouched.

Only RFC1918 IPv4 prefixes are accepted. Default routes, public ranges, loopback,
link-local, multicast, and anything overlapping the Overlay pool are rejected.

**Exporting creates reachability, not permission.** A remote Peer still has to pass the
exporting Client's [access policy](#deciding-who-reaches-you) for each target.

If two Peers export the same prefix, each Client picks the first eligible one it saw and
falls to the next if that one's last path dies. It's per-Client and not persisted, so two
of your machines may legitimately pick different exporters.

---

## Deciding who reaches you

The **Client Access Policy** is the only ACL in Lantunnel, and it lives on the machine
being reached. Not on the Gateway. Not on a server. Route selection decides *where* to
send; your Client independently decides whether to serve.

Defaults: an empty policy means **every Peer holding a profile for your Tunnel may reach
you**. Getting a profile already required you to issue one, so a second gate on top added
no boundary — it only made fresh installs mysteriously unreachable. Name one Allow rule
and it becomes the only way in. **Deny is always checked first and always wins.**

Set it in the desktop UI, or in `settings.json`:

```jsonc
{
  "client_access": {
    "allow": [
      // SSH to this machine
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // Jellyfin on the NAS beside it
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // Anything on the IoT subnet, any TCP port
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // ...and never the router, whatever the Allow list says
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

Targets are `this_peer`, `ip`, `cidr`, or `host`. Ports are `any` or `exact` — port ranges
are not supported. Rule order is meaningless; only Deny-beats-Allow matters. A rule never
names a source Peer: every authenticated member of the Tunnel gets the same answer.

To refuse everything, deny `0.0.0.0/0` and `::/0` on both TCP and UDP — that's exactly
what the UI's "block all incoming" writes, so the saved file matches what you asked for.

---

## Running on a server (headless)

`--headless` (alias `--no-ui`) runs the identical runtime with no window, tray, or WebView
— same reconnect logic, same PeerLink and relay behaviour, same SOCKS5 and TUN surfaces.

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # foreground, no UI
lantunnel-client status --json                # from another shell
lantunnel-client disconnect
```

Bare `--headless` connects the auto-connect profile, so a service unit needs no Tunnel ID:

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

Headless has no settings UI, so edit `settings.json` in the config directory directly —
see [Settings reference](#settings-reference).

**On Windows**, release builds use the GUI subsystem, so a normal launch opens no console
window and `cmd.exe` does not wait for it. When a short command's output and exit status
matter, use `start /wait`:

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## Phones

Android (`apps/android-proxy`, VpnService) and iOS (`apps/ios-proxy`, NetworkExtension)
run the same Rust core through `tp-mobile-ffi`. Import the `.peer` profile by scanning its
QR code or opening the file, then start the VPN.

There is no network-mode switch on a phone: the VPN service is the only way to reach other
apps' traffic, so native routing always follows the runtime.

---

## Command reference

### `lantunnel-client`

```
lantunnel-client                          Open the desktop UI
lantunnel-client connect <TUNNEL_ID>      Connect one imported profile, no UI
lantunnel-client disconnect               Disconnect the running Client
lantunnel-client status --json            Print status as JSON
lantunnel-client tunnel import <FILE>     Import one .peer profile
lantunnel-client tunnel list              List profiles as JSON
```

`tunnel list` prints Tunnel ID, Peer ID, Overlay IP, and bootstrap kind for each imported
profile. Private key material is not serializable and never appears.

| Option | Meaning |
|---|---|
| `--headless`, `--no-ui` | Run the full runtime without the UI |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace` |
| `--local-socks5-listen <ADDR>` | Move the loopback SOCKS5 listener |
| `--desktop-network-mode <MODE>` | `socks5_only` or `lan_routes_tun` |
| `--lan-route <CIDR>` | Install one native LAN route (repeatable) |
| `--enable-lan-p2p` | Allow LAN addresses as direct-path candidates |
| `-V`, `--help` | Version, help |

Environment overrides: `LANTUNNEL_LOCAL_SOCKS5_LISTEN`, `LANTUNNEL_DESKTOP_NETWORK_MODE`,
`LANTUNNEL_LAN_ROUTES`, `TUNNEL_PROXY_APP_CONFIG_DIR`.

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

Offline by design. It refuses symlinks and will not overwrite an existing file.

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>]              Run the Gateway
lantunnel-gateway onboard --pairing <FILE>       Onboard a Platform-managed Gateway
lantunnel-gateway mapping serve                  Standalone UDP mapping reflector
```

`--config` defaults to `configs/gateway.yaml`. `mapping serve` exists for unusual layouts;
a normal Gateway binds its own mapping socket and does not need it.

---

## Settings reference

`settings.json` in the Client config directory. Every key is optional.

| Key | Default | Meaning |
|---|---|---|
| `auto_start` | `false` | Launch at login |
| `auto_connect` | `false` | Connect on launch |
| `local_proxy_enabled` | `true` | Run the local SOCKS5 listener |
| `local_socks5_listen` | `"127.0.0.1:1080"` | Its address (loopback only) |
| `desktop_network_mode` | `"socks5_only"` | Or `"lan_routes_tun"` for native routes |
| `lan_routes` | `[]` | Native routes to install in `lan_routes_tun` mode |
| `tunnel_first` | `false` | Let Tunnel routes beat overlapping local LAN routes |
| `exported_lans` | `[]` | Private prefixes this Peer publishes |
| `auto_export_current_lan` | `true` | Also publish the networks this machine is on |
| `client_access` | open | The ACL — see [above](#deciding-who-reaches-you) |
| `p2p_allow_lan_candidates` | `false` | Offer LAN addresses as direct-path candidates |
| `log_level` | `"info"` | Client log level |

Unknown keys are rejected rather than ignored, so a typo surfaces instead of silently
doing nothing.

---

## Where files live

| What | Path |
|---|---|
| Client config, imported profiles, secrets | `~/.lantunnel/app/` (override: `TUNNEL_PROXY_APP_CONFIG_DIR`) |
| Client settings | `~/.lantunnel/app/settings.json` |
| Gateway config | `configs/gateway.yaml` (or `--config`) |
| Gateway Tunnel admission | `state/scopes.d/*.scope` |
| Gateway relay usage ledger | `state/relay-usage.wal` |
| Tunnel owner file | wherever `init-tunnel --output-dir` put it — back it up |

The imported private key is stored in an owner-only file the Client creates; it is never
written to a log, never sent to a Gateway, and never leaves the machine.

---

## Troubleshooting

**The Client won't connect.**
Check that the Gateway is running and its data port is reachable from outside
(`nc -z gw.example.com 8443`, or `nc -zu` for QUIC). Then confirm the Tunnel's `.scope`
is in the Gateway's `scopes_dir` — without it the Gateway has no reason to admit you.

**"Peer already attached" / one Client keeps getting kicked.**
Two Clients are running the same `.peer`. Issue a second profile with `add-peer`; a
profile is one device's identity, not a shared credential.

**Everything works but stays on relay.**
Look at the traffic counters in the UI — they split Direct from Relay. Symmetric NAT on
both ends can defeat hole punching. If both Peers are on the same LAN, add
`--enable-lan-p2p` so local addresses are offered as candidates. Verify UDP `8444` reaches
the Gateway; without the mapping probe neither Peer learns its public mapping.

**Direct works, relay doesn't (or the reverse).**
They are independent paths. Relay needs the Gateway's data port; direct needs UDP to flow
between the Peers. Test one at a time.

**A remote service refuses the connection.**
The target Client's access policy is refusing it — check `client_access` *on that machine*,
not on yours. A `NotAuthorized` result is final and never falls through to another Peer.

**A LAN export isn't reachable.**
The exporting Client must currently be attached to that network for the export to be
ready — a configured prefix is only published when it exactly matches a connected one.
Then check that Client's access policy allows the specific target and port.

**Version mismatch.**
Peers, Gateways, and profiles must be on the same 2.0.x line. The wire format is not
negotiated across versions; mixed deployments fail closed.

**Getting more detail.**
`--log-level debug` on the Client, `log.level: debug` in the Gateway config. Logs never
contain private keys, profile contents, or session keys.

---

## Where to go next

- **[lantunnel.app](https://lantunnel.app/)** — free Tunnel, managed Gateways, downloads,
  and guides for game streaming, private AI tooling, and home services.
- **[CONTEXT.md](../CONTEXT.md)** — how the pieces fit and what every term means.
- **[PROTOCOL.md](./PROTOCOL.md)** — the wire format, if you're implementing against it.
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** — building, testing, and sending a patch.
