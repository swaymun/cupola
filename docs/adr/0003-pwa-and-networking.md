# ADR 0003: Native apps only, no web client

Cupola has no web client. People use the native iPhone/iPad app and the
computer-side host; pairing happens only in the app (scan the code or paste the
pairing link) and with `cupola pair` on the host. The daemon's main listener is
loopback. Phones reach it through Tailscale, the home network listener (TLS
pinned to the host key, see `lan.rs`) or the relay (see `relay.rs`); each route
applies the same device-session checks. The App Server remains stdio-only.

The pairing link keeps the form `{origin}/pair#secret=…`; the app parses it and
the secret stays in the fragment, which browsers never send. A link opened
outside the app (a Camera-app scan) gets one line of plain text telling the
person to open Cupola and scan or paste it there. No route serves HTML, scripts
or other web assets, and apart from the pairing claim endpoints every API and
WebSocket request needs a paired device or the loopback capability. Network
membership alone does not pair a device.
