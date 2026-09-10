# Proposal: account-free ephemeral Tunnels

**Status:** proposed, not scheduled
**Scope:** Platform + Client. No wire-protocol change.

## The problem

Every path into Lantunnel today starts with a `.peer` profile, and every `.peer`
starts with somebody who has an account. Even the shortest route —
the hosted-Gateway quick start in [`README.md`](../../README.md) — is: create an account, verify the
email, sign in, add a Peer, connect.

That is a reasonable amount of work for a network you intend to keep. It is a
lot of work for the question most first-time users actually have, which is
*"does this even reach my NAS?"* Someone evaluating Lantunnel against a
one-command tool abandons it before the first packet.

## What Tailscale's `tailcat` does

[`tailscale/tailcat`](https://github.com/tailscale/tailcat) is the reference
point. It is Tailscale's data plane with no control plane: one side runs
`tailcat` and gets a ~58-character address, the other side connects with it, and
traffic bootstraps over a free rate-limited DERP relay before magicsock upgrades
it to a direct path. No account, no root, no routing-table changes.

Two things are worth copying and one is worth not copying.

Worth copying:

- **Zero enrolment.** The first useful thing happens before any identity exists.
- **The address is the whole handshake.** It is shared out of band, by whatever
  channel the two people already use.

Not worth copying:

- **The address is also the whole credential.** `tailcat`'s own README warns that
  anyone who learns the address of a `no-auth-ssh` server gets a shell. Lantunnel
  sells the opposite property: a Tunnel admits exactly the Peers its owner
  signed for, and a Client decides for itself what it will serve. An ephemeral
  mode has to keep that, not trade it away.

`tailcat` is also explicit that it is **not anonymous** — node public keys are
identity, and the hosted relays keep metadata logs. Any Lantunnel equivalent
should make the same statement rather than imply more.

## What this would look like here

The load-bearing discovery is that **nothing in the protocol has to change.**

- A Fleet Gateway already learns which Tunnels to admit from a `ScopeSnapshot`
  frame pushed over the Gateway-control channel, applied live by
  `ScopeStore::replace_managed_snapshot`. The Platform can mint a Tunnel at
  runtime and have a Gateway admit it without a restart, a file, or a deploy.
- Per-Tunnel relay quota is already a field in that snapshot and is already
  enforced Gateway-side. An ephemeral Tunnel gets a small one.
- The Platform already mints Tunnels programmatically for account signup.

So the shape is:

1. The Client asks the Platform for an ephemeral Tunnel. No session, no account.
2. The Platform mints a Tunnel with a short TTL and a small relay quota, issues
   the caller's `.peer`, and pushes the scope to a Fleet Gateway.
3. The Client shows a short join code. The owner sends it to whoever they want
   in the Tunnel, out of band.
4. The second Client redeems the code, gets its own `.peer`, and connects.
5. On expiry the Platform drops the Tunnel from the snapshot and both Peers stop
   attaching.

Every Peer still holds its own key and still proves possession on attachment.
The Gateway still relays ciphertext it cannot read. What changes is only who
holds the Tunnel signing key and for how long.

## Open questions

These are the decisions a real design has to make, not details to fill in later.

1. **Who mints the ephemeral Tunnel's signing key?** Platform-minted needs no
   protocol change and keeps quota, abuse controls, and revocation on the
   existing rails — but the Platform can then mint any Peer in that Tunnel.
   (This is already true of every free and managed Tunnel, so it is not a new
   trust property; it is a new *context* for one.) Client-minted is closer to
   `tailcat`'s trust model but requires the Gateway to admit a scope it was
   handed rather than one the Platform pushed, and moves rate limiting off the
   Tunnel dimension entirely.
2. **How long does an ephemeral Tunnel live?** Process lifetime, like `tailcat`?
   A fixed window measured in hours? Can it be promoted into a real Tunnel by
   signing in, so an evaluation that goes well does not have to start over?
3. **What stops abuse?** There is no account to rate-limit, meter, or ban.
   Candidates: per-IP creation limits, a small byte cap enforced by the existing
   per-Tunnel quota, a short TTL, and refusing relay entirely once direct fails
   more than N times.
4. **How strong is the join code?** It is a bearer credential for Tunnel
   membership. Single-use, short-lived, and rate-limited on redemption is the
   minimum; whether it should additionally be bound to a key the redeemer proves
   is the real question.
5. **What do we claim?** Not "anonymous". At most "no account required", with
   the same plainness `tailcat` uses about relay metadata.

## Why this is filed rather than built

The Client-side onboarding work — sign in from the app, pick a Tunnel by name,
create a Peer, connect — removes most of the friction for people who *do* want
an account, and it needs no new trust decisions. That ships first. This proposal
is the follow-on for the people who bounce before an account exists at all, and
it deserves its own review because question 1 above is a security decision, not
an implementation detail.

## References

- [`tailscale/tailcat`](https://github.com/tailscale/tailcat) — architecture and
  the explicit "not anonymous" and "address is the credential" warnings.
- [`CONTEXT.md`](../../CONTEXT.md) — Tunnel, Scope, Peer, and Gateway vocabulary.
- [`docs/PROTOCOL.md`](../PROTOCOL.md) — the wire format this proposal does not
  change.
