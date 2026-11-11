# ADR 0002: Rust daemon, thin menu client, and child-process App Server

`wonderd` owns state, persistence, API, supervision, and the agent harness. The native menu client is presentation and control only. Wonder launches `codex app-server --stdio` as a direct child and does not depend on Codex internal Rust crates, daemon, proxy, remote-control pairing, or public App Server sockets.
