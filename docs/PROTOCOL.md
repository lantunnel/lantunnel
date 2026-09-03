# Wire Protocol

The normative description of the Lantunnel wire format.

The implementation lives in `crates/tp-core/src/protocol.rs`, and it is the final authority.
Any change to `MsgType`, field order, or a primitive encoding **must** update this file in the
same commit.

**Current protocol version: `4`.** Lantunnel 2.0 makes no compatibility promise to any 1.x
implementation. Existing transport framing and proxy fast paths are unchanged; V2 adds only
the fields and messages described below.

If you are implementing against this, read [Framing](#framing) and
[Message types](#message-types) first, then the V2 sections — those are where the
authentication and sealing live.

**Contents**

- [Framing](#framing)
- [Primitive types](#primitive-types)
- [Message types](#message-types)
- [Payload messages](#payload-messages)
- [QUIC TCP flow streams](#quic-tcp-flow-streams)
- [Sealed record format](#sealed-record-format)
- [P2P scalar values](#p2p-scalar-values)
- [Peer attachments](#peer-attachments)
- [Capability negotiation](#capability-negotiation)
- [V2 attachment admission](#v2-attachment-admission)
- [V2 signed offer and answer](#v2-signed-offer-and-answer)
- [V2 encrypted relay control](#v2-encrypted-relay-control)
- [Compatibility rules](#compatibility-rules)

---

## Framing

`tp_core::protocol::pack()` produces a `BinaryMessage` body:

```text
[version:u8][msg_type:u8][message fields...]
```

All integer fields are big-endian. String and byte-string lengths are unsigned big-endian
values.

Transport framing is a separate layer:

| Transport | Outer frame |
|---|---|
| QUIC stream | `[len:u32 BE][BinaryMessage body]` |
| QUIC datagram | `BinaryMessage body` |
| WebSocket | one binary frame containing `BinaryMessage body` |
| gRPC | `StreamMessage.data = BinaryMessage body` |

`Data` and `UdpData` may be carried internally as `(header, payload)` chunks for zero-copy
stream writes, but the on-wire bytes are identical to the contiguous body layout above.

## Primitive types

| Type | Encoding |
|---|---|
| `u8` | one byte |
| `i8` | one byte, signed |
| `u16` | two bytes, big-endian |
| `u32` | four bytes, big-endian |
| `i64` | eight bytes, big-endian |
| `bool` | `0x01` for true, `0x00` for false |
| `string` | `[len:u16 BE][UTF-8 bytes]` |
| `conn_id` | exactly 12 bytes, UTF-8 truncated or zero-padded |
| `session_id` | exactly 16 bytes |
| `cert_fp` | exactly 32 bytes |
| `payload` | the remaining bytes after the fixed and header fields |

## Message types

### Core

| Value | Name | Fields, in wire order |
|---:|---|---|
| `0x01` | `Connect` | `conn_id`, `network:string`, `address:string` |
| `0x02` | `ConnectResponse` | `conn_id`, `success:bool`, `error:string` |
| `0x03` | `Close` | `conn_id` |
| `0x04` | _reserved_ | Retired with Gateway port forwarding. **Never reuse.** |
| `0x05` | _reserved_ | Retired with Gateway port forwarding. **Never reuse.** |
| `0x06` | `Auth` | `client_id:string`, `group_id:string`, `username:string`, `password:string`, `group_password:string` |
| `0x07` | `AuthResponse` | `status:string`, `reason:string` |
| `0x08` | `Error` | `message:string` |
| `0x09` | `Heartbeat` | `client_id:string`, `timestamp:i64` |
| `0x0A` | `HeartbeatAck` | `timestamp:i64` |

### Data

| Value | Name | Fields, in wire order |
|---:|---|---|
| `0x10` | `Data` | `conn_id`, `payload:bytes` |
| `0x11` | `UdpData` | `conn_id`, `payload:bytes` |
| `0x12` | `UdpFragment` | `conn_id`, `frag_id:u32`, `index:u8`, `total:u8`, `payload:bytes` (20-byte header) |

### P2P signaling

| Value | Name | Fields, in wire order |
|---:|---|---|
| `0x20` | `P2pAnnounce` | `client_id:string`, `group_id:string`, `local_count:u8`, repeated `(ip:string, port:u16)`, `nat_hint:u8`, `cert_fp` |
| `0x21` | `P2pAnnounceAck` | `public_ip:string`, `public_port:u16`, `server_time_ms:i64` |
| `0x22` | `P2pOffer` | `session_id`, `src_client_id:string`, `dst_client_id:string`, `candidate_count:u8`, repeated `(ip:string, port:u16, kind:u8)`, `src_cert_fp:cert_fp`, `role:u8` |
| `0x23` | `P2pAnswer` | `session_id`, `accepted_client_id:string`, `ok:bool`, `reason:string`, `candidate_count:u8`, repeated `(ip:string, port:u16, kind:u8)`, `dst_cert_fp:cert_fp` |
| `0x24` | `P2pPunchSync` | `session_id`, `t_start_ms:i64`, `burst_count:u8`, `offset_count:u8`, repeated `offset:i8` |
| `0x25` | `P2pProbe` | `session_id`, `seq:u32`, `sent_ms:i64` |
| `0x26` | `P2pProbeAck` | `session_id`, `seq:u32`, `recv_ms:i64` |
| `0x27` | `P2pSessionReady` | `session_id`, `rtt_us:u32`, `chosen_remote_ip:string`, `chosen_remote_port:u16` |
| `0x28` | `P2pTeardown` | `session_id`, `reason:u8` |
| `0x29` | `P2pPeerHint` | `peer_client_id:string` |

### Relay routing and V2

| Value | Name | Fields, in wire order |
|---:|---|---|
| `0x2A` | `RelayRouteBind` | see `crates/tp-core/src/protocol.rs:725` (pack) / `:1142` (unpack) |
| `0x2B` | `RelayRouteBindAck` | see `crates/tp-core/src/protocol.rs:735` (pack) / `:1146` (unpack) |
| `0x2C` | `AuthV2Challenge` | `challenge:32 bytes` |
| `0x2D` | `AuthV2Proof` | `tunnel_id:string`, `peer_id:string`, `overlay_ipv4:4 bytes`, `peer_public_key:string`, `membership_signature:string`, `attachment_signature:string` |
| `0x2E` | `P2pOfferV2` | `source_peer_id:string`, `target_peer_id:string`, `signed_offer:remaining bytes` |
| `0x2F` | `P2pAnswerV2` | `source_peer_id:string`, `target_peer_id:string`, `signed_answer:remaining bytes` |
| `0x30` | `EncryptedPeerControlV2` | `target_peer_id:string`, `peerlink_session_id:16 bytes`, `conn_id:12 raw bytes`, `flags:u8`, `sealed:remaining bytes` |

### A note on `P2pPeerHint`

`P2pPeerHint` is byte-for-byte unchanged in protocol v4. Its complete body is
`[0x04][0x29][peer_client_id:string]`, with no LAN-address tail.

Trusted Peer LAN host alias ownership travels only in the additive Platform heartbeat JSON
contract. The same IP may independently appear in `P2pAnnounce` or in an offer/answer as a
link candidate, but that binary signaling never establishes route ownership. This feature
therefore changes neither the binary transport format nor `PROTOCOL_VERSION`.

## Payload messages

`Data` and `UdpData` payloads are **not** length-prefixed inside `BinaryMessage`; they consume
the remaining bytes in the frame.

With the current 12-byte `conn_id`, both have a 14-byte tunnel header before the payload
(`version + msg_type + conn_id`).

UDP target addressing is carried by the initial `Connect { network: "udp", address }` for the
association — not by each `UdpData` frame.

## QUIC TCP flow streams

QUIC keeps one bidirectional stream per TCP flow. The first item on that stream is
length-prefixed with the transport's existing `u32` frame length. Its body is one of:

```text
TcpFlowOpenV1 = 1:u8 || conn_id[12] || network:string || address:string
TcpFlowOpenV2 = 2:u8 || conn_id[12] || peerlink_session_id[16] || sealed_open
```

A `TcpFlowOpenV1` opener waits for a framed `ConnectResponse` before returning the stream. A
`TcpFlowOpenV2` raw open returns as soon as it has written the complete opaque preface and
does not parse a response. On receive, the transport surfaces the complete V2 preface together
with the raw stream.

For a relayed V2 flow the Gateway reads only `version` and `conn_id`, consumes the already
bound exact route, writes the complete preface through unchanged, and continues its existing
64 KiB metered `copy_bidirectional`. It does not decode `peerlink_session_id`, `sealed_open`,
or any subsequent record. QUIC FIN and half-close remain the flow shutdown signal.

No new capability bit is needed. Both endpoints already negotiated the existing
`tcp_flow_stream_v1` QUIC facility, and V2 selection happens only for an authenticated
PeerLink route.

## Sealed record format

V2 endpoint records use the existing stream frame helper with the wire body:

```text
nonce[24] || ciphertext || tag[16]
```

The endpoint AEAD code limits each plaintext record to the existing 64 KiB bridge chunk.

**This format adds no sequence number, replay window, rekey, ACK, FIN, or timer protocol.**

That omission is deliberate, and worth stating plainly, because a reader auditing the AEAD in
isolation will look for a replay counter and not find one. Every V2 record travels inside an
authenticated QUIC or TLS connection, and those transports already provide anti-replay,
ordering, and integrity for the bytes they carry. A second replay window at the record layer
would duplicate a guarantee the carrier already makes, not strengthen it. The nonces are
random rather than counter-derived for the same reason: the endpoint AEAD authenticates one
record and its routing context, and leaves freshness to the layer that owns the connection.

The contrapositive is the part to keep in mind. **These records are only safe to carry over a
transport that supplies that guarantee.** Relaying a sealed V2 record over a datagram path
with no anti-replay, or replaying one into a different session, is outside what this
construction defends against.

### Sender identity rewriting

For `EncryptedPeerControlV2`, `target_peer_id` names the destination on sender ingress. After
authenticating and routing the frame, the Gateway rewrites **only that outer field** to the
authenticated sender's stable Peer ID. The receiver therefore sees its remote Peer identity
for PeerLink and AAD selection. `sealed` is forwarded byte-for-byte unchanged.

## P2P scalar values

| Type | Values |
|---|---|
| `nat_hint` | `0=Unknown`, `1=FullCone`, `2=Restricted`, `3=PortRestricted`, `4=Symmetric` |
| `candidate.kind` | `1=Host`, `2=ServerReflexive` |
| `role` | `1=Initiator`, `2=Acceptor` |
| `teardown.reason` | `1=Idle`, `2=HealthFail`, `3=User`, `4=FatalError` |

The `role` scalar describes only the initiator/acceptor relationship for one PeerLink
negotiation. It is not a product or Tunnel-session role.

## Peer attachments

Every V2 endpoint is a symmetric Peer. For each attachment, a `lantunnel-client` selects a
signed `.peer` identity, attaches to its Tunnel through a Gateway, exposes its mandatory
loopback-only SOCKS5 NO AUTH listener, and may both originate and accept policy-authorized TCP
and UDP flows. There is no public `client`/`app` product-role selector and no role-based
routing authority.

The carrier bootstrap retains legacy-shaped fields for wire stability. **Those fields confer
no identity and select no behavior** — stable Peer identity comes only from the signed
membership and attachment proof described below.

## Capability negotiation

WebSocket and gRPC negotiate transport capabilities in their existing outer handshake, because
they do not exchange the in-band `Auth` / `AuthResponse` messages that QUIC uses:

- WebSocket: the optional `X-TP-Transport-Capabilities` request header and HTTP 101 response
  header.
- gRPC: the optional `transport-capabilities` request and response metadata entry.

Each value is the decimal representation of the existing protocol-v4 Auth capability byte. It
is not a new `BinaryMessage` field.

| Bit | Capability |
|---|---|
| `0x01` | `route_bind_control_v1` |
| `0x02` | `tcp_flow_stream_v1` |
| `0x04` | `relay_source_attestation_v1` |
| `0x08` | `peer_mesh_v2` |

WebSocket and gRPC advertise `0x05` by default for the existing path; a V2 Client explicitly
adds `0x08`. Route-bind control frames use their main reliable ordered stream when no separate
QUIC control lane exists, while the QUIC-only TCP flow stream API (`0x02`) stays disabled
there.

The client request is an **offer**. The server response is the **intersection** of that offer
with the server's supported capabilities, and both session objects use only the negotiated
intersection. A missing or invalid value decodes as zero; unknown bits are ignored.

The public `lantunnel-gateway` is V2-only: every carrier must negotiate `peer_mesh_v2`, or
admission fails before session registration. There is no legacy Gateway constructor and no
compatibility harness.

## V2 attachment admission

`peer_mesh_v2` uses the existing carrier authentication surface only to open a **bounded
pending session**. That initial surface carries the existing `tunnel_id`, `client_id` (a
runtime Replica handle), the capability byte, and empty legacy credential fields. It does not
add a stable Peer ID to the legacy `Auth` layout. A pending session is not registered and can
neither proxy nor signal.

The public Gateway config has no `auth_username`, `auth_password`, `credential`, `proxy`,
`tunnel_key`, `group`, or `password` authority. Its management router does not mount legacy
credential registration or deletion, peer-join registration, or TUIC UUID routes. Tunnel
admission comes only from a verified static or signed managed V2 Scope.

The exchange:

1. The Gateway sends one fresh `AuthV2Challenge`.
2. The Client replies with `AuthV2Proof`, containing the fields of the Tunnel-signed public
   membership plus a signature made by the Peer private key over the fixed attachment-proof
   domain, the challenge, the Tunnel ID, the stable Peer ID, the runtime Replica ID, and the
   membership hash.
3. The Gateway verifies the membership using the public issuer key in the installed `.scope`,
   then verifies the Peer's proof of possession.
4. Only after **both** checks pass does it register the attachment.

A challenge is bound to that pending session, has a bounded deadline, and is never persisted
or reused.

The receiver reconstructs the canonical membership bytes from these fields. That encoding and
the attachment-proof transcript are defined by `tp_core::provisioning`; their cross-language
golden fixtures are normative.

## V2 signed offer and answer

`P2pOfferV2` and `P2pAnswerV2` are independent of the earlier P2P message types. The signed
body consumes the remainder of the frame, must contain **1–65536 bytes**, and has no second
length prefix. Its canonical signed content is defined by `tp_core::peer_link_crypto` and is
verified **only by the endpoint**.

The two Peer IDs before the signed body are Gateway delivery metadata:

- On an inbound message, `source_peer_id` is **untrusted**. The Gateway replaces it with the
  stable Peer ID established by AuthV2.
- The Gateway resolves `target_peer_id` only to an authenticated V2 attachment in the same
  Tunnel, then forwards the signed body byte-for-byte.
- The Gateway does not decode, verify, or rewrite that body, and creates no signaling session
  record.
- An unavailable target produces the existing `Error` message. **The Gateway never fabricates
  a signed `P2pAnswerV2` on a Peer's behalf.**

## V2 encrypted relay control

`EncryptedPeerControlV2` carries endpoint-sealed OPEN, OPEN response, fatal route abort, and
zero-`conn_id` PeerLink/gossip control.

The only defined flag is `0x01 = route_abort`. Unknown flag bits, an empty sealed body, and a
sealed body larger than 65536 bytes are all rejected. The Gateway treats `sealed` as opaque
and does not parse, rewrite, sequence, replay-filter, or rekey it.

**Nonzero `conn_id`.** The source first completes the existing `RelayRouteBind` /
`RelayRouteBindAck` exchange using the stable V2 target Peer ID. The first non-abort encrypted
control consumes that bound route and marks it sealed-v2. Later control is accepted only from
one of the two authenticated endpoints named by the same-Tunnel exact route, and only when
`target_peer_id` names the actual opposite endpoint. `route_abort` is forwarded and then
removes the route; ordinary `Close` retains the existing one-way FIN and two-way cleanup
semantics.

**Zero `conn_id`.** Exact-forwarded to an authenticated same-Tunnel Peer without creating or
consuming a flow route. It cannot carry `route_abort`.

## Compatibility rules

### Encoding

- Do not reorder existing fields.
- Do not reuse removed message type values.
- Additive fields require a protocol version bump, unless every peer can infer absence from
  the remaining payload length.
- `conn_id` values longer than 12 bytes are truncated on write.
- Readers stop `conn_id` at the first zero byte; embedded zero bytes are not valid in
  practice.
- P2P local-address, candidate, and port-offset lists use `u8` count fields. The Rust writer
  emits at most 255 entries for these lists.
- `AuthResponse.status` uses the values `"success"` and `"failed"`.

### Bootstrap

- QUIC retains the in-band `Auth` / `AuthResponse` carrier bootstrap.
- WebSocket and gRPC retain their outer header/metadata bootstrap instead of exchanging those
  in-band messages.
- A V2 Client sends the frozen legacy credential fields empty. The public Gateway requires
  `peer_mesh_v2` and rejects any non-empty legacy credential field before registration. Any
  role-shaped compatibility field is ignored as routing or identity authority.

### Versions

- The current Rust parser accepts only protocol version `4`; anything else is rejected with
  `ProtoError::BadVersion`.
- Protocol v4 peers must not assume a runtime downgrade to v3. A mixed-version rollout
  requires deploying components that agree on `PROTOCOL_VERSION`.
