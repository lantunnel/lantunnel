# Architecture and Concepts

This is the map of Lantunnel 2.0: what the pieces are, what each one is allowed to know,
and why the boundaries fall where they do.

It is not a wire specification — that is [`docs/PROTOCOL.md`](./docs/PROTOCOL.md) — and it
is not a tutorial — that is [`docs/USAGE.md`](./docs/USAGE.md). Where this document and the
implementation disagree, the implementation is right and this file has a bug.

Read it if you are reviewing a change, self-hosting a Gateway, implementing against the
protocol, or trying to understand why a design decision went the way it did.

**Contents**

- [The shape of the system](#the-shape-of-the-system)
- [The three programs](#the-three-programs)
- [Identity and provisioning](#identity-and-provisioning)
- [Gateway runtime](#gateway-runtime)
- [Managed mode](#managed-mode)
- [Peer connectivity](#peer-connectivity)
- [Routing and gossip](#routing-and-gossip)
- [Local access control](#local-access-control)
- [What the Client reports](#what-the-client-reports)
- [Invariants](#invariants)
- [Talking about performance honestly](#talking-about-performance-honestly)
- [Vocabulary](#vocabulary)

---

## The shape of the system

A **Tunnel** is one mutually trusted mesh of **Peers** and one routing namespace. It
outlives every process: Platform, Gateway, and Client all come and go, the Tunnel does not.

Every V2 endpoint is a symmetric Peer. There is no client/server split, no `app` role, and
no per-endpoint privilege — any Peer can originate a flow and any Peer can accept one,
subject only to the target's own policy.

```
                      ┌───────────── Tunnel ─────────────┐
                      │                                  │
      ┌───────────────┴──┐                        ┌──────┴───────────────┐
      │      Peer A      │◀──── Direct QUIC ─────▶│       Peer B         │
      │  Overlay .0.3    │       (preferred)      │   Overlay .0.7       │
      │  exports         │                        │   exports            │
      │  192.168.1.0/24  │                        │   10.0.0.0/24        │
      └───────┬──────────┘                        └──────────┬───────────┘
              │                                              │
              │        ┌────────────────────────────┐        │
              └───────▶│         Gateway            │◀───────┘
       Attachment      │  • admits by .scope        │   Attachment
       + Encrypted     │  • NAT hints + signaling   │   + Encrypted
         Relay         │  • opaque sealed relay     │     Relay
                       └────────────────────────────┘
                        holds no Peer keys, no plaintext,
                        no durable Peer or route state
```

Three properties hold everywhere in the design:

1. **Authority follows the private key.** A Peer proves possession of its own Ed25519 key on
   every attachment. Nothing else — no password, no group secret, no bearer token — grants
   membership, and no component can act on a Peer's behalf.
2. **The Gateway is a meeting point, not a trusted party.** It learns who is attached and
   how many bytes moved. It does not learn what moved.
3. **Policy lives at the destination.** The machine being reached decides whether to serve.
   Routing answers *where*; the target Client independently answers *whether*.

---

## The three programs

### `lantunnel-client`

The only public Client product, built from `apps/lantunnel-client`. Running it normally
starts the desktop UI; `--headless` (alias `--no-ui`) runs the same runtime with no window,
tray, or WebView. Both share the same Peer identity, settings, secret storage, and status
meanings — headless is not a stripped-down second product.

Every successful `.peer` connection begins PeerLink and mesh reconciliation. The mesh is
always on: there is no enable/disable flag and no persisted setting for it. The separate
LAN link-candidate opt-in controls only *candidate discovery*, never the mesh itself.

The loopback local SOCKS5 ingress authenticates against the verified Peer identity and
never falls back to a shared secret.

`lantunnel-tun-helper` is an internal helper binary, not a fourth product.

### `lantunnel-gateway`

The Gateway process, from `apps/lantunnel-gateway`. Rendezvous, NAT traversal and
signaling, exact encrypted relay, and relay usage accounting.

The public Gateway is **V2-only**. Its configuration has no `auth_username`,
`auth_password`, `credential`, `proxy`, `tunnel_key`, `group`, or `password` authority. Its
management router mounts no legacy credential registration, no peer-join registration, and
no TUIC UUID routes. Every carrier must negotiate `peer_mesh_v2` or admission fails before
session registration. There is no legacy constructor and no compatibility harness.

Tunnel admission comes from exactly one place: a verified static or signed managed
**Scope**.

### `lantunnel-admin`

The offline owner tool, from `apps/lantunnel-admin`. Two commands, `init-tunnel` and
`add-peer`, and no network access at all. It is not a controller, and there is no
request/approve/renew workflow.

---

## Identity and provisioning

### The files

| File | Sensitivity | Contents |
|---|---|---|
| `.tunnel` | **Owner only**, mode `0600` | Random Tunnel ID, Tunnel signing private key, static Gateway endpoint and TLS trust, allocated Peer public identities. No transport or group shared secrets. |
| `.scope` | **Public** | Version, Tunnel ID, Tunnel signing public key. That is the entire payload. |
| `.peer` | **One device only** | The Peer's private key and its public membership, the Tunnel signing public key, the Overlay `/32`, a replica fan-out count, and exactly one bootstrap — static Gateway facts, or a managed `platform_url`. |

A `.scope` cannot issue Peers and carries no membership secret, Peer state, LAN state, or
ACL. Handing one to a Gateway Operator grants them nothing beyond the ability to admit that
Tunnel's attachments.

Lantunnel 2.0 adds no `.meta` sidecar and no profile password; the `.tunnel` and `.peer`
files are protected by file ownership and mode.

### The commands

**`init-tunnel`** takes the Gateway transport, a host and/or IP, a port, a TLS server name,
and optionally a full self-signed or private-CA PEM. It writes one `.tunnel` and one
`.scope`.

**`add-peer`** locks one `.tunnel`, allocates or validates an address in `198.18.0.0/16`,
creates a Peer keypair and signed membership, atomically updates the allocation list, and
writes one `.peer`.

The replica fan-out count in a `.peer` is editable and is neither identity nor commercial
authorization — it is a runtime concurrency hint.

> **One `.peer`, one Client instance.** A profile is a device identity, not a shared
> credential. Copying one onto a second machine does not create a second Peer; the two
> instances contend for the same identity. The acceptance harnesses reject duplicate paths,
> file identities, stable Peer IDs, and Overlay IPs before starting any Client.

### The identities

**Peer ID** — a stable opaque identity within one Tunnel, signed together with the Peer
public key and Overlay IP by the Tunnel signing key.

**Overlay IP** — one Tunnel-unique IPv4 `/32` from the fixed `198.18.0.0/16` pool, shared by
all of a Peer's transport replicas. One OS network namespace may activate one 2.0 Tunnel in
this release.

**Public Peer Membership** — the minimal Tunnel-signed binding
`(tunnel_id, peer_id, overlay_ip, peer_public_key)`. Gateway attachments and PeerLinks both
require proof of possession of the matching private key; the membership alone is not a
credential.

**Replica** — one live Gateway transport instance belonging to a Peer. One Client connect
lifecycle generates one random runtime family in the exact format
`{tunnel_id}-{8 ASCII alphanumeric}-{replica_index}`. Retries within that lifecycle reuse
the family; a fresh user connect after a full disconnect generates a new one. A Replica is
not a Peer and carries no membership of its own.

**Tunnel Signing Key** — an Ed25519 keypair created once, by `init-tunnel` or by Platform
Create Tunnel. The private key stays in `.tunnel` (or in encrypted managed owner state) and
signs Peer memberships. The public key is copied into `.scope` and `.peer` so Gateways,
Platform, and Peers can verify them. Rotating Gateways, Regions, Scopes, or Peers never
replaces it. Losing or compromising it means a **new Tunnel ID and new profiles** — there is
no in-place issuer rotation protocol.

### Who is who

- **Tunnel Owner** creates the Tunnel, keeps the `.tunnel`, issues `.peer` files, and gives
  the public `.scope` to a Gateway Operator. In managed modes the Platform does this on the
  owner's behalf.
- **Gateway Operator** runs a Gateway, installs static `.scope` files, sets machine resource
  limits, and operates TLS identity, service units, metrics, logs, and the optional usage
  WAL. They never receive a `.tunnel` or any Peer private key.
- **Peer User** imports one `.peer`, runs the Client, and owns that Client's access policy,
  LAN exports, Tunnel First setting, and local tools.
- **Platform Operator** runs the hosted service: accounts, managed provisioning, bound and
  fleet Gateways, relay usage, support. Self-hosted and hand-shared Tunnels never appear
  there.

---

## Gateway runtime

The Gateway is a rendezvous, NAT-traversal and signaling service, exact encrypted relay, and
usage meter. It holds **no** durable managed Scope, Peer, gossip, LAN, active/standby, or
route state.

### Scope Map

An in-memory `tunnel_id → issuer_public_key` map. Static entries load from one `scopes.d`
directory. Managed entries converge idempotently to the desired snapshot delivered over the
Gateway's control connection.

Each delivery is a **complete** managed snapshot: the Gateway atomically replaces its managed
subset, and a missing entry expresses deletion. It reports `gateway_state` back as evidence
of application, and repeats that state periodically as liveness.

Removing a Scope closes that Tunnel's attachments and relay — but never a healthy direct
PeerLink, which no longer depends on the Gateway.

A collision between a static and a managed Scope carrying the same Tunnel ID is rejected.

### Attachment and admission

A **Gateway Attachment** is a replaceable connection from one Peer to the Gateway, used for
admission, contact hints, NAT mapping, signaling, and relay. Losing it stops relay and new
hole punching; it does not by itself end a healthy direct PeerLink.

Admission runs in two stages, and this split is the point:

1. **Carrier bootstrap** opens a *pending* session using the existing Auth layout: Tunnel ID,
   a `client_id` that is only a runtime Replica handle, the capability byte, and empty legacy
   credential fields. A pending session is not registered. It cannot proxy and cannot signal.
2. **Proof.** The Gateway sends one fresh `AuthV2Challenge`. The Client answers with
   `AuthV2Proof` carrying its Tunnel-signed public membership plus a signature over the fixed
   attachment-proof domain, the challenge, Tunnel ID, stable Peer ID, Replica ID, and
   membership hash. The Gateway verifies the membership against the issuer key in the
   installed `.scope`, then verifies possession of the Peer key. Only then does it register
   the attachment.

The challenge is bound to that pending session, has a bounded deadline, and is never
persisted or reused. Nothing in the legacy-shaped carrier fields is trusted as identity.

After proof, the Gateway atomically leases one active runtime family per
`tunnel_id + stable_peer_id`. Replicas in that family are admitted up to
`max_replicas_per_peer`; a different family, or a duplicate exact Replica ID, is rejected
without evicting the healthy family. Each permit releases on disconnect and the family lease
leaves with its last Replica.

This is what rejects an accidental duplicate Client start — independently started Clients
pick different random families. It is a collision guard, not device identity, and it is not
a defense against someone who holds the Peer private key and deliberately reuses the active
family with different indices.

### Peer Registry and Contact Hints

A bounded, process-local registry used for NAT endpoint observation, signaling, and exact
relay. V2 adds Tunnel-first lookup and authenticated stable Peer IDs. It is not a membership
database.

A **Contact Hint** is a bounded observation that another authenticated Peer in the same
Tunnel is currently attached. It can start PeerLink reconciliation. It never creates a LAN
route, a gossip record, membership truth, or an export order.

---

## Managed mode

Everything above works standalone. Managed mode adds an optional control plane so a hosted
Platform can place Tunnels on Gateways without any Peer key or plaintext ever reaching it.

The Platform itself is a separate, closed-source service and is not in this repository. What
follows is the Gateway and Client side of the contract — which is what Lantunnel implements
and what a self-hosted deployment needs to understand.

### Control session

One authenticated **outbound** WSS connection, initiated by the Gateway to the Platform
origin and owned by that binding's Durable Object. The Gateway authenticates by proving
possession of the persistent self-signed leaf key recorded at onboarding.

The Durable Object is the live session coordinator: it sends the current desired full Scope
snapshot and receives `gateway_state` evidence and usage reports. It is not a second durable
database and has no command queue. Durable desired state lives in D1.

**The Platform never dials the Gateway.** There is no inbound management port, no management
hostname, and no polling protocol.

Gateway-side implementation: `apps/lantunnel-gateway/src/gateway_control.rs`.

### Bound Gateway (BYOG)

A user-owned Gateway bound once to a Platform account by an opaque binding ID. The owner
supplies immutable public data-connection facts and uses a one-time pairing artifact to
onboard. The Gateway generates and owns one persistent P-256 private key and self-signed
leaf for its exact public IP; the key never leaves the machine, while the Platform records
the verified leaf PEM.

Binding facts and the pinned leaf are immutable in this release. Changing them, approaching
expiry, or recovering from key compromise means onboarding a replacement and explicitly
moving Tunnels — there is no automatic renewal and no in-place rotation controller.

The owner must explicitly accept each Tunnel. Accepting a Tunnel implicitly accepts its
issuer-signed Peers; there is no per-Client list.

Removing an unreachable bound Gateway is a local soft-delete, never a cascade delete of
Tunnels. Referencing Tunnels stay unavailable until their owners choose another accepted
Gateway. Revoking the binding immediately rejects its reconnect and all new resolves;
stopping the Gateway and deleting its local pairing state is the operator's hard stop.

### Resolution

The connect-time step for a managed `.peer`. The Client signs a resolve request with its Peer
private key. The Platform verifies the Tunnel-signed membership and possession proof, reuses
the Tunnel's current healthy Gateway or selects the bound BYOG Gateway (or one healthy fully
managed Gateway across permitted Regions), and commits that Gateway's public `.scope` into
the authoritative desired snapshot.

Resolve returns immutable connection facts and the exact leaf PEM **only when** fresh
`gateway_state` evidence for the current Gateway process matches the current desired
snapshot. Otherwise it nudges the Durable Object to reconcile and returns a retryable pending
result, and the Client retries with a newly signed request.

One nullable current-Gateway field on the Tunnel keeps cold-start Peers co-located. A single
conditional database update resolves concurrent cold starts; losers reread the winner and
write nothing. Placement is never written into `.peer`, and there is no lease or
active-active protocol. A disable or placement change commits a new snapshot and immediately
fences new resolve facts.

### The twelve decisions

These are the stable identifiers for managed-Gateway identity and control. Changing any of
them is a design change, not a local implementation detail.

| | Decision |
|---|---|
| **D1** | **Owner inputs.** BYOG accepts one public IPv4/IPv6 address, a data transport, a data port, and the UDP mapping port its host reflects on. Management hostname, port, SNI, certificate, and private key are not owner inputs. |
| **D2** | **One control route.** Exactly one outbound WSS connection per managed Gateway, to the Platform public origin, owned by that binding's Durable Object. The Platform never dials a Gateway management port and assigns no Gateway hostname. |
| **D3** | **Mapping port.** Every Gateway runs its own mapping service and binds that UDP socket itself — there is no second process and no startup ordering. The port is a registration fact defaulting to `8444`: the operator records it, the Platform stores it, and managed resolve hands it to Clients, which probe there rather than assuming a constant. Several Gateways may share a host, each with its own data and mapping port. QUIC data cannot share the mapping port; a TCP carrier may reuse the number for WebSocket/gRPC, though a distinct data port is recommended.<br><br>*Superseded:* the mapping service was once a single machine-wide reflector on a fixed UDP/8444, excluded from owner input, run as a separate `mapping serve` process the Gateway refused to start without. |
| **D4** | **Firewall and NAT.** Exactly two inbound paths: public Internet to the mapping UDP port, and public Internet to the selected data port and protocol. Plus ordinary outbound TCP/443 to the Platform origin. There is no inbound managed control port. |
| **D5** | **Local key lifecycle.** The Gateway locally creates and owns one persistent P-256 TLS private key and self-signed leaf with its exact immutable public IP as an IP SAN. That leaf serves the data listener, and its key proves Gateway identity on the outbound control connection. The private key never leaves the machine. A separate one-time pairing artifact lets the Platform verify and record the leaf; the Platform never emits or rewrites the operator's runtime configuration. |
| **D6** | **TLS trust.** Managed resolve returns the exact stored leaf PEM. The Client dials the immutable public IP, treats that IP as the TLS server identity, requires the exact IP SAN, and trusts only that PEM. Managed data TLS has no DNS name, no SNI route, no shared private root, and no fail-open mode. The outbound control connection independently verifies the Platform origin with system PKI. |
| **D7** | **Honest readiness.** An authenticated control session proves control reachability. Gateway readiness on that channel proves the local data protocol can use the persisted leaf and that the mapping listener is bound. Only a real Client data handshake proves public data-path reachability, and only a real Client mapping probe proves public UDP/NAT reachability. |
| **D8** | **Onboarding commit point.** One Gateway-initiated transaction, not separate test and complete phases. Its durable result is the immutable binding facts and verified leaf; retries reuse that binding rather than creating a second. |
| **D9** | **Immutable facts.** Address, transport, data port, leaf and key, and IP SAN are immutable. Changing one means a new binding and an explicit Tunnel move. |
| **D10** | **Fleet reuse.** The Platform fleet uses the same binary, identity model, control protocol, desired-state contract, and Client PEM pin. It stays Platform-owned, with its own Region health and Tunnel-level assignment; it does not become BYOG. |
| **D11** | **Expiry replacement.** No automatic renewal, no in-place rotation. Onboard a replacement before expiry or after compromise, then move Tunnels explicitly. Client facts are never silently repinned. |
| **D12** | **Replica wire stability.** The Replica ID grammar `{tunnel_id}-{8 alphanumeric family seed}-{decimal index}` is unchanged; P2P and relay establishment may continue parsing it. |

---

## Peer connectivity

### PeerLink

The mutually authenticated logical relationship between exactly two stable Peers.
Independent signed offer and answer messages prove Tunnel membership and Peer key
possession, bind the direct QUIC certificate fingerprint, and derive directional relay
sealing keys.

A PeerLink is not a transport connection and not a carrier state machine. Each unordered
Peer pair has at most one, and it can move between lanes without changing Peer identity.

### Two lanes

**Direct Lane** — an end-to-end Peer-to-Peer QUIC carrier. Preferred for new flows and for
gossip when healthy, and it survives Gateway failure entirely.

**Encrypted Relay Lane** — an exact-peer fallback through the Gateway session, sealed at
three seams:

1. `EncryptedPeerControlV2` carries sealed OPEN, OPEN response, and gossip control on the
   reliable control path. Its only outer action bit is `route_abort`, for fatal
   endpoint-requested teardown; ordinary `Close` keeps the existing one-way FIN and two-way
   cleanup.
2. `Data` and `UdpData` keep their message type, outer connection ID, queues, backpressure,
   datagram scheduler, fragmentation, and reassembly. Only the payload is sealed.
3. QUIC TCP keeps one stream per flow through an additive sealed flow-stream v2. Peers seal
   OPEN and stream records at the codec boundary while the Gateway continues an opaque
   `copy_bidirectional`.

The sealing is a fixed XChaCha20-Poly1305 construction with random nonces. **It has no
sequence number, replay window, or rekey protocol, and that is deliberate.** Every sealed
record travels inside an authenticated QUIC or TLS connection that already provides
anti-replay, ordering, and integrity; a second replay window at the record layer would
duplicate that guarantee, not strengthen it.

The honest consequence: relay payloads get confidentiality and per-record authenticity, but
not freshness, ordering, or exactly-once delivery against an *active* Gateway. Valid old TCP
records or UDP datagrams could still be replayed or reordered by one. These records are only
safe on a carrier that supplies those properties itself.

All KDF and AAD inputs use one length-prefixed canonical encoding with separate domains for
control, framed payload, TCP open, TCP open response, and TCP data. The Gateway consumes a
bound TCP route using only the outer version and connection ID; the PeerLink session ID stays
an end-to-end input for key selection and AEAD verification, never Gateway route state.

Producers seal framed payloads before freezing their owned buffers, and sealed TCP codecs
reuse one record buffer per flow, so encryption adds neither a carrier downgrade nor a
per-record allocation.

### Candidates and Flows

A **Link Candidate** is a temporary local or server-reflexive address used only for direct
hole punching. It is never a routable Overlay or LAN identity.

A **Flow** is one TCP connection or UDP association, pinned at open time to one target Peer
and one lane. New flows prefer Direct. Existing flows are **not** migrated when a better lane
appears — the choice is fixed for the life of the flow.

---

## Routing and gossip

### How Peers learn about each other

**Peer Runtime Record** — one Peer's bounded full-replacement record carrying its current LAN
subnet exports. Overlay identity comes from signed membership, not from here. The record's
bytes are hashed for repair, and it is accepted only from the current authenticated PeerLink
to that origin. No third party may advertise on a Peer's behalf.

**Peer Gossip Directory** — one Client's process-local collection of remote runtime records.
Rebuilt after restart, never persisted by Client, Gateway, or Platform.

**Gossip Digest** — a compact record hash each Client sends on each ready PeerLink roughly
every 25–35 seconds. It detects a missed update and requests a full replacement. It is not
liveness, not version ordering, and not global agreement.

### LAN exports

A **LAN Subnet Export** is a locally connected RFC1918 IPv4 prefix a Peer publishes to declare
that it can proxy authorized TCP/UDP targets behind it. Two independent sources on one Client:
the prefixes its owner typed, and — while **Export Current LAN** is on — the networks the
Client is currently attached to.

Export Current LAN is on by default and re-derives on every interface scan, so the export
follows the machine between home, office, and café. Turning it off withdraws only what it
added; the typed list is unaffected either way. The default is deliberate: the overwhelmingly
common reason to install a Client is to reach the machines beside it, and the Tunnel is still
the boundary — only Peers issued a profile for it can use the export.

Rejected outright: default and public prefixes, anything overlapping the Overlay pool,
loopback, link-local, multicast, unspecified, and protected control targets.

**An export creates reachability, not permission.** The target still passes the exporting
Client's access policy.

### Choosing between exporters

**Local Export Order** is one Client's in-memory, first-seen order of currently eligible remote
Peers publishing the same prefix. Never persisted; it may legitimately differ across Clients
or change after restart.

The **Locally Active Exporter** ("Active here") is the first eligible Peer in that order,
selected for a new flow. The **Local Standby Exporter** ("Standby here") is any later one:
when the active Peer's final lane fails, the next local candidate is selected, and a
returning Peer enters at the tail.

Neither is a Tunnel-wide owner. There is no global FIFO, no Gateway election, and no lease.
Both names are deliberately scoped — say "Active here", never "the Active Peer".

**RouteMatcher** resolves an exact Overlay or LAN prefix to one Peer before the selected
PeerLink chooses a lane. It is not a fastest-response fan-out.

### Native routing

**Native routing** is the desktop setting that decides whether this machine installs real
routes for the Tunnel, so every application on it reaches Peers directly rather than only the
apps pointed at the loopback SOCKS5 ingress. A phone has no equivalent: its VPN service is the
only way to reach other apps' traffic, so native routing there follows the runtime and has no
switch.

**Tunnel First** is a separate local setting that allows valid remote Tunnel routes to win
over overlapping connected LAN routes. Gateway, control, DNS, metadata, and self-export
destinations stay protected native routes. Tunnel First *ranks* the routes native routing
installs; it never decides whether they are installed at all. The two were split precisely
because conflating them made one switch answer two unrelated questions.

**Native Routing Ready** is a backend state projected from the actual TUN/native apply result.
Failed or unapplied routes cannot be reported ready, and process liveness alone is never
sufficient. There is no route ledger or acknowledgement protocol.

---

## Local access control

The **Client Access Policy** is the only user ACL in Lantunnel 2.0. It is stored only on the
target Client and protects both that Peer and the LANs exported through it.

A **Client Access Rule** matches one target (this Peer, an IP, a CIDR, or a host), a protocol,
and a port that is `Any` or `Exact(u16)`. Port ranges are not a 2.0 capability.

- An empty Allow list means every Peer in the Tunnel may reach this Client. Reaching it
  already required an issued profile for the same Tunnel, so a second gate added no boundary —
  it only made fresh installs silently unreachable. Naming anything in Allow makes it the only
  way in.
- **Deny is always checked first and is never overridden.** List order carries no meaning.
- A "this Peer" Allow maps the Peer's Overlay `/32` to loopback at the same port by default,
  and may carry one explicit exact local `forward_to`. The rule authorizes the requested
  Overlay endpoint and its bound final mapping as one pair; the final target is not silently
  added to the independently requestable allow set.
- LAN targets must belong to a currently ready local export.
- A rule never selects a source Peer. Every authenticated member of the Tunnel gets the same
  answer.
- A fully closed Client is spelled out — Deny `0.0.0.0/0` and `::/0` on both protocols — rather
  than implied, so what is saved matches what the owner asked for.

The **Client Access Check** compiles that policy into the existing host filter and final target
check. New TCP and UDP associations validate the requested and final target before the dial
handler runs. It is a narrow adapter, not a second proxy stack.

Gateway and Platform never receive the access policy and never decide active/standby.

---

## What the Client reports

**Peer Connection View** — the backend-owned local list of known remote Peers, including
authenticated-ready, authenticating, syncing, stale, and unavailable rows. Only authenticated
ready records participate in routing and export selection. It is not a global online list.

**Mesh Healthy** — a backend state meaning initial full sync completed and locally known live
Peers have authenticated usable PeerLinks. It does not mean every Peer has an identical view,
and it is not the same as "Gateway connected".

**Traffic Counters** — payload-only Direct and Relay counters. Gossip, heartbeats, and control
frames are excluded. The Gateway separately counts encrypted relay bytes for accounting.

Frontends render backend enums and reason codes. They never derive Mesh Healthy or Native
Routing Ready from counts, settings, or process liveness.

---

## Invariants

- One `.tunnel` creates one `.scope` and many `.peer` files.
- A Gateway loads static `.scope` files from one `scopes.d` and receives managed Scope bytes
  into the same in-memory map. Managed desired state and cleanup obligations are durable only
  in the Platform database — never as Gateway files.
- A `.peer` imports into exactly one Client instance, has one stable Peer ID and one Overlay
  `/32`, and may own multiple Replicas and lanes within that Client's active runtime family.
- Every QUIC, WebSocket, and gRPC session records the exact resolved Gateway `SocketAddr` the
  Client actually dialed. For hostname-based gRPC, a pinned connector dials that address while
  the URI authority and TLS server name remain independent inputs. P2P bootstrap derives both
  the mapping-reflector address and physical-underlay discovery from this value, so a
  placeholder such as `0.0.0.0:0` is never a valid connected session fact.
- A static `.peer` contains Gateway connection facts. A managed `.peer` contains only
  `platform_url` and never binds Peer identity to a Gateway, Region, IP, hostname, or
  certificate.
- Managed Tunnel creation has no Region selector. Fully managed reuses one current healthy
  Gateway for all Peers and selects across permitted Regions only when needed; BYOG uses its
  one explicit accepted placement.
- Key roles are disjoint and none substitutes for or derives another: the Gateway P-256 key
  terminates data TLS and proves Gateway identity without leaving the machine; the Platform TLS
  identity authenticates the control server; the Tunnel Ed25519 key signs memberships; each
  Peer Ed25519 key proves that Peer's possession.
- Each unordered Peer pair has at most one logical bidirectional PeerLink, and it may use
  Direct or Encrypted Relay without changing Peer identity.
- Gateway restart discards attachments, seed state, signaling, and relay — but does not close
  healthy direct PeerLinks.
- Peer runtime records, learned Peers, remote exports, and Local Export Order are process-local
  and are rebuilt by full sync and digest repair.
- Route selection answers *where to send*; the target Client Access Check independently answers
  *whether to serve*. A `NotAuthorized` result is final and never triggers a standby attempt.

---

## Talking about performance honestly

Numbers from this system are easy to misread, so the project holds itself to a few rules.

- **WAN throughput and loss are bounded by the underlay, not by the tunnel.** A run that
  saturates at the uplink rate has found the link, not a Lantunnel ceiling, and must not be
  cited as a transport limit. On an asymmetric consumer connection the upstream is usually the
  binding constraint.
- **A transport ceiling may only be claimed** from a path whose measured underlay capacity
  exceeds the offered load, or alongside an explicitly recorded underlay baseline from the same
  run.
- **Relay-only or single-machine container evidence cannot close a performance claim** on its
  own. Direct and relay are different paths and have to be measured as such — direct on a real
  physical LAN, relay through a Gateway configured to refuse direct signaling, with both
  Clients fully restarted so no stale PeerLink can satisfy the measurement.
- **Scale matters to the conclusion.** Realtime game traffic is typically well under 1 Mbps and
  sits far below any interesting knee. Game *streaming* (Moonlight class, 20–100 Mbps) does not
  fit a typical consumer uplink and is a Direct-only workload there.

---

## Vocabulary

Terms carry weight here. Several of them name a boundary, and using a looser synonym quietly
claims something the system does not do.

| Use | Not | Because |
|---|---|---|
| **Peer** | client, app, replica, role | V2 endpoints are symmetric. `client`/`app` survive only as historical labels in legacy and test code; `app` is never a product or binary. |
| **Gateway Attachment** | Peer identity, mesh lifetime | An attachment is replaceable and short-lived; identity is not. |
| **PeerLink** | connection, socket, four-message handshake | It is a logical relationship between two Peers, not a transport. |
| **Contact Hint** | membership snapshot, route snapshot | A hint is a bounded observation, not truth. |
| **Gossip Digest** | Gateway announce, global sequence | Digests repair missed updates; they do not order or agree. |
| **Encrypted Relay** | trusted relay, generic envelope | Plain "Relay" may describe the physical path — never its confidentiality boundary. |
| **Active here / Standby here** | the Active Peer, lease holder | Export order is per-Client. Nothing about it is global. |
| **Network Scope** | policy grant, route database, issuer bundle | A `.scope` is a Tunnel ID and a public key. It grants nothing else. |
| **Tunnel Owner File** | Gateway scope, authority database | The `.tunnel` is offline owner state, not a runtime service. |
| **Peer Profile** | invitation, enrollment request, bearer key | A `.peer` is one device's identity, not a joinable credential. |
| **Overlay IP** | link candidate, LAN IP | Overlay addresses are routable identity; candidates are transient. |
| **Native routing** | Tunnel First | One installs routes, the other ranks them. |
| **Client Access Policy** | Gateway policy, host policy, source ACL | The only ACL, and it lives on the target machine. |
