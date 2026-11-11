# Outside agents

Cupola exposes its Project thread tools to agents it did not launch: Claude Code
or Codex in a terminal, and bots with MCP Streamable HTTP and OAuth support.
Use Cupola's existing Tailscale Serve address followed by `/mcp`:

```text
https://<your-mac>.<your-tailnet>.ts.net:<existing-serve-port>/mcp
```

The agent's computer must belong to the tailnet and its ACLs must permit this
address. The daemon keeps its existing loopback listener; this feature creates
no listener and does not configure Tailscale. Use Serve, never Funnel. Check
`tailscale serve status` before using the endpoint. On the Mac itself,
`http://127.0.0.1:<daemon-port>/mcp` also works. Credentials are bound to the exact
origin and `/mcp` audience: switching between local and tailnet addresses requires
another sign-in. Only the configured HTTPS `.ts.net` hostname and explicit
loopback hosts are accepted; forwarded headers cannot choose an issuer.

## Approve a sign-in

The agent discovers OAuth from the endpoint's `401` challenge, registers a public
client and opens Cupola's sign-in page. The page names the agent and displays the
complete return address. Client names are self-reported. Verify the address:
loopback returns to the computer opening the browser page (paste the final URL
back to a headless agent on another computer); an HTTPS address gives the
service at that address the granted access. Only approve a sign-in you started.

Enter a fresh pairing code obtained on the Mac. This feature's codes are separate
from phone pairing codes. Each code lasts five minutes and approves one sign-in;
requesting another code invalidates the previous one. The browser's consent form
is also protected by an expiring request and an HttpOnly, SameSite cookie.

Read only is selected by default, even when the agent requests every supported
scope. The owner can separately allow:

| Scope | Tools |
| --- | --- |
| `read` | `wonder_project_list`, `wonder_thread_list`, `wonder_thread_read`, `wonder_thread_wait`, `wonder_provider_list`, `wonder_model_list` |
| `start` | `wonder_thread_start`, including its first message |
| `message` | `wonder_thread_send`, including steering where supported |
| `stop` | `wonder_thread_stop` |
| `simulator` | `wonder_simulator_list`, `wonder_simulator_settings`, `wonder_simulator_screenshot`, `wonder_simulator_apps` (read-only); `wonder_simulator_set_appearance`, `wonder_simulator_set_content_size`, `wonder_simulator_set_language`, `wonder_simulator_reset_permissions`, `wonder_simulator_open_url` |

Simulator tools act only on this Mac's simulators that are already running;
they never boot or shut one down, and they are the same typed actions the phone's
Simulator view uses (fixed `simctl` arguments, validated, with read-back). A
screenshot comes back as MCP image content (PNG) with its size and SHA-256.
Agents cannot tap, swipe or type in a simulator: input needs the phone's control
lease, and `wonder_simulator_list` says so in `agentInput`. Cupola's own "Wonder
Control" test simulators are refused while a `scripts/wonderctl` lease is active.

Access covers all included Projects. Start/message/stop do not imply each other.
Tools are listed according to the granted scopes, and each call checks its scope.
Starting or messaging a thread can run an agent on the Mac. The permission ceiling
is separate: Read only, Workspace with approval, or Full access. A new thread
cannot exceed that ceiling; messaging and stopping wider-permission threads are
refused. Providers whose permissions Cupola cannot constrain (currently OpenCode)
require an explicit Full access grant for direct MCP operations. OAuth scopes
are not propagated into the normal Cupola tools used by started or steered
agents; native delegation keeps its existing rules. A strict permission ceiling
across downstream delegation, particularly to fixed-permission providers, still
needs policy integration and verification before release. Workspace threads started here always ask
for approval. Model catalogs are the daemon's cached catalogs; listing them does
not start a model. Waits are bounded to 30 seconds and never stop work.

**Settings UI is still needed.** This backend card does not add Mac or iOS
controls, so a normal owner cannot yet obtain these new codes from Settings.
The Mac Settings follow-up must show/copy the existing tailnet MCP URL, request and
display a fresh code with its expiry, list approved agents with return addresses,
scopes, permission ceiling and expiry, and revoke a selected client with loading
and error feedback. Do not present this as a shipped owner setup flow until that
UI exists. Revocation must be available on the Mac even when the agent is offline.

For a local developer fixture, the corresponding capability-protected APIs are:

- `POST /api/v1/mcp/pairing-code` → `{pairingCode, expiresAtMs}`
- `GET /api/v1/mcp/clients` → clients and nonsecret grant metadata
- `DELETE /api/v1/mcp/clients/{client_id}` → revoke that client immediately

These require the existing `x-wonder-loopback-capability` header and a loopback
host. OAuth tokens and phone sessions cannot call them. Never log codes or copy
credentials into issue reports. The HTTP test fixture obtains the capability from
its own in-memory daemon state; it never uses the installed app's credentials.

## Claude Code

Run on a computer that can reach the chosen Cupola address:

```sh
claude mcp add --transport http cupola https://<wonder-tailnet-address>/mcp
claude mcp login cupola
```

For a headless terminal, use `claude mcp login cupola --no-browser` in an
interactive terminal (`ssh -t` over SSH), open the
printed sign-in URL on a computer that can reach Cupola, approve with the Mac's
code and paste the final callback URL into the terminal. `claude mcp list` checks
the connection. `claude mcp logout cupola` removes the local credential; revoke
on the Mac to invalidate the client's server-side grants as well.

## Codex

```sh
codex mcp add cupola --url https://<wonder-tailnet-address>/mcp
codex mcp login cupola
```

Codex may start OAuth during `mcp add`; if it completes there, a second login is
unnecessary. Use `codex mcp login cupola --no-browser` for the printed-URL flow
on an already configured server. To request
optional write scopes explicitly, use
`codex mcp login cupola --scopes read,start,message,stop,simulator`; the owner still chooses
which scopes to approve. `codex mcp list` shows the configured server. Log out with
`codex mcp logout cupola` and revoke on the Mac to cut off the server-side access.
These commands follow the [official Codex MCP documentation](https://developers.openai.com/codex/mcp).

## ChatGPT and hosted bots

A tailnet URL by itself is unreachable from an ordinary hosted ChatGPT MCP
connection: the user's browser being on the tailnet does not put the hosted MCP
client there. A bot running on a tailnet machine can connect normally with OAuth.
ChatGPT requires a separately authorized private connectivity integration with
access to this tailnet, such as OpenAI's [Secure MCP Tunnel](https://developers.openai.com/blog/connect-private-mcp-servers-to-openai-products).
A bridge would also need verification of Host, issuer, audience and OAuth callback
handling with this endpoint. That compatibility is unverified. This card does not
provision a bridge, enable Funnel, or expose Cupola publicly.
Do not promise ChatGPT connectivity merely because the sign-in page opens locally.

An eligible hosted client uses the MCP URL and OAuth authentication. Dynamic
registration means no preconfigured client ID or secret is needed. The owner must
inspect the hosted service's return address on the sign-in page before approving.
Agents that lack OAuth/S256 PKCE cannot connect.

## Authentication and storage

- Protected-resource metadata: `/.well-known/oauth-protected-resource/mcp` (also
  `/.well-known/oauth-protected-resource`).
- Authorization-server metadata: `/.well-known/oauth-authorization-server`.
- Dynamic public-client registration: `POST /oauth/mcp/register`.
- Browser consent: `GET`/`POST /oauth/mcp/authorize`.
- Authorization-code exchange and refresh: `POST /oauth/mcp/token`.
- MCP: `POST /mcp`, using stateless Streamable HTTP JSON responses. Notifications
  return `202`; authenticated `GET`/`DELETE` return `405` because there is no SSE
  event stream or server session to terminate.

Authorization codes expire after 60 seconds and are consumed on the first
exchange attempt, including an incorrect PKCE verifier or client. Redirects must
match registration (HTTP loopback allows a different port); exchange must repeat
the exact approved redirect. S256 is mandatory; implicit/password grants and
client secrets are not supported. A supplied resource must match the exact MCP
endpoint; omitted resource binds to that endpoint for client compatibility.

Access tokens expire after ten minutes. Refresh tokens rotate on every use and
expire with the original thirty-day grant; refresh does not extend that deadline.
Replaying a used refresh token revokes its entire grant family, including newly
issued access tokens. Refresh may narrow scopes but cannot restore or add them.
A fresh authorization creates a new grant; it does not cancel older grants for
the same client. Refresh scope narrowing affects its own grant family. To reduce
all previously granted access, revoke the client and register/sign in again with
the narrower permissions. Client revocation invalidates all its grants and pending
authorization codes.
Phone/device revocation is separate. Resetting the workspace atomically removes
all MCP clients, grants, access/refresh tokens, authorization codes and Mac pairing
codes. Old credentials are refused immediately; outside agents must register and
obtain fresh owner consent after a reset.

Every MCP request checks current expiry, audience, grant and client revocation in
SQLite; OAuth credentials cannot authenticate the phone API or a Cupola-started
runtime. A request already accepted may have queued work before revocation;
revoking access does not cancel work already running.

SQLite stores SHA-256 hashes of access tokens, refresh tokens, authorization codes
and Mac pairing codes. Only nonsecret client metadata, redirects, scopes and
permission ceilings are stored in clear text. Unapproved consent requests and
CSRF hashes are held in bounded daemon memory; restart cancels those requests.
Registration and approval attempts are rate limited. Protect the database, local
Mac capability and agent credential store: OAuth does not protect a compromised
Mac, and approved agents can read Project messages under `read` scope.

## Verification

The protocol/security tests use HTTP requests against a daemon fixture and a
synthetic provider, with no live model work. The opt-in interoperability check
runs the installed CLIs with temporary config directories and `--no-browser`
(Claude uses a private pseudoterminal; Codex accepts a piped callback). Codex
configuration models a starting daemon
to defer its automatic browser login; explicit login uses the real OAuth routes:

```sh
CARGO_BUILD_JOBS=4 cargo test -p wonderd --lib mcp
CARGO_BUILD_JOBS=4 cargo test -p wonderd --lib thread_tools
CARGO_BUILD_JOBS=4 cargo test -p wonder-store
CARGO_BUILD_JOBS=4 cargo test -p wonderd --lib mcp_cli_oauth_login_against_loopback_fixture -- --ignored --nocapture
```

The browser page's rendered appearance, Settings approval/revocation controls,
and hosted ChatGPT connectivity need their own acceptance evidence. The HTTP
fixture does not establish those outcomes.
