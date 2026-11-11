# Relay encryption

Cupola's relay is a Cloudflare Tunnel per computer, provisioned by the push
Worker (`services/push`, `/v1/relay/*`) and run by `cloudflared` under the host
daemon (`crates/wonderd/src/relay.rs`). The Worker only provisions tunnels and
tokens; requests travel through Cloudflare's tunnel edge, which ends TLS. That
edge, not the Worker, is the party that must see only ciphertext.

## What crosses the relay

Only `/api/v1/rpc/sealed`. Two independent layers enforce it:

- The tunnel's ingress rule names that path (`RELAY_SEALED_PATH` in
  `services/push/src/cloudflare.ts`); the edge answers 404 to anything else.
- The host answers 404 to every other path whose `Host`, URI authority or
  `Origin` names the relay address (`relay::refuses`, an allow-list). Calls the
  sealed channel routes internally carry a request extension the network cannot
  set (`rpc::server::SealedCall`), so they pass.

Pairing, events, artifacts, MCP and plaintext RPC never use the relay address.
A phone pairs on the home network or Tailscale; the relay origin then appears in
`host.status` `routes`, and everything it does there runs inside the channel. The
phone's ordinary HTTP API calls travel as `http.request` inside it, so they
pass the same session, CSRF and signature checks as at home; live events use
`events.subscribe`.

## The channel

`crates/wonderd/src/rpc/sealed.rs`, documented for clients in
[RPC.md](RPC.md#sealed-transport):

1. Client hello: a fresh X25519 key.
2. Host welcome: its fresh X25519 key and a `HostProof`, the host key's P-256
   signature over the dialed origin and `SHA-256("wonder-sealed-v1\n" ‖ both
   keys)`. The phone checks it against the host key pin it saved at pairing.
3. HKDF-SHA256 (salt: that hash) over the X25519 secret gives one
   ChaCha20-Poly1305 key per direction. Every later frame is sealed with the
   nonce `0 ‖ u64 frame number` in its direction.
4. The first sealed frame is the device session; then `auth.challenge` and
   `initialize`, signed by the device key, as on any route.

The device key is a Secure Enclave signing key that cannot do key agreement, so
this is not Noise IK: the host authenticates by signature over the ephemeral
keys, the device by its session and challenge signature inside the channel.

## Properties

- **Confidentiality and integrity:** the edge sees two handshake messages
  (public keys and the proof) and opaque frames. No session, CSRF token,
  method, result or event crosses in the clear (rpc test
  `the_relay_edge_sees_only_ciphertext_while_the_device_contract_holds_sealed`).
- **Impersonation:** a host without the pinned key, or a proof for another
  origin, fails at the phone (`HostProof`).
- **Replay and tampering:** within a connection a frame opens only at its own
  position in its own direction, so an altered, reordered, replayed or
  reflected frame closes the socket. Across connections both sides' keys are
  fresh, so a recorded frame or welcome opens nowhere else
  (`a_frame_replayed_into_another_connection_does_not_open`). The host's own
  Worker requests carry a timestamp and single-use nonce (`replayed_proof`).
- **Forward secrecy:** channel keys live only in memory for one connection.
- **Rotation:** channel keys rotate on every connection. The long-term
  credentials are the ones pairing creates: the host key the phone pins, and
  the device key and session. Re-pairing creates a new device record and
  session; revoking the old device (`cupola devices revoke`) drops its
  sessions, ends its open sealed sockets at once and refuses new ones. A phone
  that sees a different host key is told to pair again.

## Limits on unauthenticated callers

Anyone can reach the relay address (hostnames are public through certificate
transparency) and the handshake needs no credential, so it is bounded before
any session exists: 32 handshakes at once, at most 4 per client address
(`CF-Connecting-IP`, believed only on the relay address; IPv6 by /64), 10
seconds to finish, a 4 KiB handshake message, and a WebSocket frame cap equal
to the message cap so a promised oversized frame ends the socket at its header
(the library default would buffer 16 MiB first). The address the Worker
returns is adopted only when it is `https://r-<label>.<zone>` for this host's
own label (`relay::own_address`). After sign-in, a connection's unread answers
are capped at 128 MiB (`MAX_QUEUED_REPLY_BYTES`); event frames count against the same
budget but wait for room instead of failing.

## Turning it on

The owner allows the relay on the computer (`cupola relay on`, RPC `relay.set`,
loopback only). A signed-in phone then gets a one-time grant from the Worker
(`POST /v1/relay/grants`) and hands it to the host with a device-signed
`relay.enable`; the host refuses it while the owner has not allowed the relay.
The daemon then runs `cloudflared` and reconnects at the same address after a
restart without a new grant. `cupola relay off` stops the tunnel, releases the
address with a host-signed disable and clears the setting.

## Limits

- The ingress rule points at `127.0.0.1:3777`; a host listening elsewhere is
  not reachable through the relay.
- `cloudflared` is not bundled; the host looks on `PATH`, Homebrew and
  `/usr/bin`, or `WONDER_CLOUDFLARED`, each time the tunnel starts. Without it
  the address is kept, and `cupola relay on` after installing it starts the
  tunnel without a new grant.
- Traffic metadata (timing, sizes, the relay hostname) is visible to Cloudflare.
