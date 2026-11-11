# Cupola notification and remote-access service

This Worker carries encrypted reply, approval and question previews through APNs,
with generic fallback alerts. Chat traffic bypasses this notification handler; optional remote access is described below. Push and Live Activities
require a Cupola account; pairing, the home network and Tailscale stay account-free. The official app can use the official provider with any
compatible Mac host, including a source-built host.

## Accounts

An account is a random ID and creation date with linked sign-in identities.
Sign in with Apple is the only method today; `identities(provider, subject_hash)`
leaves room for another. The Apple user ID (`sub`) is stored only as an
HMAC-SHA256 keyed by the `ACCOUNT_PEPPER` secret. Name and email are never requested.

- `POST /v1/auth/apple` `{identityToken, nonce, deviceKey, signature}` verifies the
  identity token against Apple's JWKS (`RS256`, issuer `https://appleid.apple.com`,
  audience in `APPLE_AUDIENCES` (the Cupola bundle ID), unexpired, and
  `nonce` claim = hex SHA-256 of the request nonce). `signature` is the device
  key's P-256 signature over `signin|apple|<nonce>`. It returns a session.
- Sessions are bound to that device key, one per key: a one-hour access token
  and a 90-day refresh token, both stored as hashes. `POST /v1/auth/refresh`
  `{sessionId, refreshToken, timestamp, signature}` needs the device key's signature
  over `refresh|<sessionId>|<timestamp>` (within five minutes) and rotates both.
- `POST /v1/challenges` needs `Authorization: Bearer <access token>`; the
  registration it leads to belongs to that account and session. Send, status,
  activity and revoke work only while the session exists; `status` returns
  `{id, account: true}`, which the Mac checks before storing a capability.
- `POST /v1/auth/signout` removes the session with its registrations, receipts and
  challenges. `POST /v1/account/delete` removes the account, every session,
  registration, receipt, challenge and identity. `POST /v1/account` returns the
  creation date.
- `GET /privacy` is the plain-language privacy page linked from the app.

Sign-in is limited to 20 per IP hash and 2,000 service-wide per hour, refresh to
30 per session per hour. Expired sessions are swept hourly with what they own.

An independently signed iOS build needs its own Apple App ID with Push
Notifications, a matching entitlement/provisioning profile, an APNs authentication
key, and a Worker deployment. APNs topic, Apple team and environment must match.
An App Store Connect upload key is not an APNs key.

## Deploy

`scripts/deploy-push` deploys in one command with no hand-editing. It refuses a
checkout that has uncommitted changes or is behind `origin/main`, then runs `npm ci`,
`npm run check` and `npm test`, applies the D1 migrations remotely and deploys.
`--dry-run` stops after `wrangler deploy --dry-run` and changes nothing remote.
`npm run deploy` and `npm run dry-run` call the same script.

The tracked `wrangler.jsonc` keeps `database_id` as a placeholder. The script reads
the real ID from `wrangler d1 list --json` by the config's `database_name`
(`wonder-push`) and writes a temporary, gitignored `wrangler.deploy.jsonc`. If
`services/push/wrangler.local.jsonc` exists (gitignored), the script uses that
complete config instead.

The official Worker serves `https://r-api.getcupola.app`. The existing
`https://wonder-push.saimun-shahee.workers.dev` endpoint remains enabled for
installed clients; both origins serve the same push, account and relay paths.
Only reserved `r-*` first-level names are used in the relay zone. The apex and
other DNS records are outside this service’s ownership.
The privacy response uses `Cache-Control: no-transform` to prevent automatic
Cloudflare Web Analytics injection on the custom domain.

```sh
scripts/deploy-push --dry-run
scripts/deploy-push
```

## Deploy your own

Use Node/npm and your Cloudflare account. Copy `wrangler.jsonc` to
`services/push/wrangler.local.jsonc`, choose your Worker/database names, then create
a D1 database and put its ID in that config. Remove or replace the official
`routes` custom domain with one you own. Set `APNS_TOPIC` to the iOS bundle ID
and `APNS_ENVIRONMENT` to `production` for TestFlight/distribution or `sandbox`
for development-signed installs.

```sh
cd services/push
npm ci
npx wrangler login
npx wrangler d1 create your-wonder-push
# Edit database_id, Worker name, topic and environment in wrangler.local.jsonc.
../../scripts/deploy-push
npx wrangler secret put APNS_KEY_ID --config wrangler.local.jsonc
npx wrangler secret put APNS_TEAM_ID --config wrangler.local.jsonc
npx wrangler secret put APNS_PRIVATE_KEY --config wrangler.local.jsonc < /private/path/AuthKey_KEYID.p8
# Supply a cryptographically random, private rate-limit hashing secret.
npx wrangler secret put RATE_SECRET --config wrangler.local.jsonc
# And a separate random pepper for account identity hashes. Never rotate it
# casually: a new pepper makes every existing account unreachable.
openssl rand -base64 32 | npx wrangler secret put ACCOUNT_PEPPER --config wrangler.local.jsonc
```

Run the script from a clean checkout that includes `origin/main`. Pass
`--config wrangler.local.jsonc` to any other wrangler command. Never
commit key files, `.dev.vars`, or secret values. Set the Mac's
`WONDER_PUSH_ENDPOINT=https://your-worker.your-subdomain.workers.dev`,
`WONDER_PUSH_TOPIC`, and `WONDER_PUSH_ENVIRONMENT` before launch. The endpoint must
be an HTTPS origin, without credentials, a path, query or fragment.

In the phone's connection settings, turn on **Notifications**. Enrollment
requires a signed challenge delivered to that specific phone through APNs. The
public HTTP challenge endpoint alone cannot authorize alert delivery. The app
retains a separate signing key and sending capability for each paired Mac. The
Mac verifies provider registration before storing the capability. There is no
shared credential embedded in an app binary.

## Delivery and retention

The Mac durably records completion/attention intent with the corresponding event
or question transaction, then distributes it to authorized devices. Notification
payloads contain generic fallback text, random registration/routing identifiers
and optional AES-256-GCM ciphertext. Only the Mac and phone hold the per-registration
preview key. A native notification service extension decrypts on the phone, even
when the app is closed. The Worker neither decrypts nor persists previews. Parent
conversation lookup remains on the Mac and is scoped to the requesting device.
Group workers and verified subagents do not generate duplicate completion alerts;
attention routes through their persisted parent ownership.

## Live Activities

The phone shows ONE Live Activity for all its computers, merged here (the
design follows T3 Code's relay; `src/activity.ts`). Each computer reports only
its own chats, in full, through `POST /v1/registrations/{id}/activity` with the
registration's sender capability: `{rows:[{routeId,title,project,status,startedAt}]}`
(at most 20; status is running, needsApproval, needsInput, done, failed or
stopped, which is kept as done). The Worker keeps those rows per registration
(`activity_rows`, migration 0006), rebuilds ONE combined state on every report
and sends one APNs `liveactivity` push to the phone's card: up to 5 rows (most
urgent first, then "+N more"), with fixed statuses. Running rows expire after
2 hours without a refresh, waiting rows after 24 hours and a finished chat
stays 15 minutes; the reaper deletes what the aggregate already ignores. When
nothing is running the card ends itself and drops after the finished window.
Identical states are not pushed again (heartbeat every 4 minutes keeps iOS from
marking the card stale), and updates never alert.

The phone owns its card's state through
`POST /v1/registrations/{id}/activity-state` with the same capability:
`update` and `start` register ActivityKit tokens (the update reply carries the
combined state), `ended` forgets one, `off` deletes the phone's rows and card,
`dismissed` and `foreground` set and clear the dismissal gate. Dismissed means
the owner swiped the card away: the Worker stops updates and push-to-start
for that phone until the owner opens Cupola again, and never starts a card
from a background update. A push APNs answers with 410 (a card swiped while
Cupola was suspended, or ended by the system after 8 hours) sets the same gate.
A push-to-start waits 10 seconds for a phone in front to start its own card.

iOS renders Live Activity payloads itself, so unlike notification previews they
cannot be encrypted. Each push carries the combined state: counts, up to five
rows with a chat title and Project name (each cut to 40 and 24 characters), a
status, a start time and a random route ID that only the paired phone can resolve
with its computer. No provider, message, command, file name or conversation ID.
The Worker stores those rows only while the card needs them and only for a phone
that registered a Live Activity token (retention above; privacy page in
`src/accounts.ts`). They are deleted when a computer stops reporting a working
chat, when Live Activities are turned off, a computer is unpaired, the phone
signs out or the account is deleted.

Previews use the exact completed turn or still-pending request. They are bounded
excerpts, not complete transcripts or attachments. Missing keys and authentication
failures retain the generic fallback. iOS controls lock-screen preview visibility.

Cupola suppresses banners and sounds whenever it is in the foreground. A signed
per-device heartbeat, sent to every enabled paired Mac every 15 seconds, suppresses
new and queued deliveries there too. Foreground arrivals stay suppressed after
leaving the app. Leases expire after 45 seconds if the phone disconnects; a background
transition clears them immediately when reachable. A network race may still send
an in-flight push, but iOS foreground presentation remains suppressed.

Source-built apps must embed `WonderNotificationService` and sign both App IDs.
Set `WONDER_APP_BUNDLE_IDENTIFIER` for both targets and retain the matching shared
Keychain group. Only preview keys enter that group; existing pairing credentials
stay in the app's original private group. APNs authentication keys stay in the Worker.

Registration renews on token changes and at least every 30 days when the app is
active. Enable it in the foreground: APNs background delivery is not guaranteed.
The toggle persists intent before connecting and has no connection/success subtext.
Temporary setup failures retry while foregrounded with exponential delays capped
at five minutes; reopening retries immediately. Permission, signing-identity or
configuration failures that need action turn the preference off and show an alert.
Turning off cancels setup immediately and queues revocation across disconnects.
Revocation is per
registration; a Mac device revocation also cancels durable deliveries and queues
provider revocation. Unknown/stale tokens fail closed.

The Worker bounds enrollment and delivery requests, deduplicates delivery intent,
and permits at most five send attempts. APNs collapse IDs reduce duplicate visible
alerts after an uncertain transport outcome; this is not an exactly-once delivery
guarantee. The Mac retries with bounded backoff for at most 24 hours. Routes and
intent history expire after seven days.

Provider metadata is limited to:

- accounts (random ID, creation date), peppered Apple user ID hashes, and device
  sessions (device public key, hashed access/refresh tokens);
- APNs tokens, device public keys and hashes of per-device sending capabilities;
- ten-minute enrollment challenges;
- opaque event/routing IDs, delivery state, attempts and timing retained seven days;
- salted IP hashes in Cloudflare's short-lived edge limiter, and opaque/token-hash
  quota counters in D1, removed after seven days without use;
- registrations removed after 90 days without renewal.

Hourly cleanup expires records. Workers observability/logging is disabled in the
template. Cloudflare and Apple still process network/delivery metadata under their
own services. Account-level logs/backups have separate retention; review them when
deploying your own service. No conversation text, file, screenshot, email account,
local Mac address or model credential is stored by this service.

## Cost and limits

The ordinary successful delivery performs four D1 row writes: one device quota,
one global quota, one receipt claim, and one completion. Expiring that receipt
adds one write. Duplicate delivery requests read the receipt without writing it
or spending another delivery quota. Enrollment, revocation, failed/retried sends,
and quota reclamation add work beyond this five-write lifetime budget.

Migration `0002_push_write_budget.sql` preserves receipts and current quota counts.
It uses `WITHOUT ROWID` tables to avoid duplicate primary-key index writes, and
reuses quota rows across hours. The hourly receipt sweep scans the seven-day
retention window instead of maintaining an expiry index on every delivery.
At 10 million sends per 30 days this is about 2.33 million retained receipts and
1.68 billion scanned rows per month. The local workerd scale test exercises that
full window; it does not certify production latency or burst capacity. Revisit
partitioning/indexing if production sweep time approaches the D1 query limit.

IP abuse is gated at 30 requests/minute by the native `IP_RATE_LIMITER` binding.
This gate is approximate and per Cloudflare location, not a global accounting
limit. Exact D1 quotas remain 60 send attempts/device/hour, 30,000 service-wide
attempts/hour, 300 enrollment challenges/hour and three challenges/token/hour.
Reusing a rate-limit namespace shares its counters; use a distinct `namespace_id`
when deploying another independent push service in the same Cloudflare account.

On the September 2026 Workers Standard pricing, 10 million successful sends use
the included 50 million D1 writes, before the extra work described above. The
$5/month account plan includes 10 million Worker requests and 30 million CPU-ms;
at an **assumed**, unmeasured 5 CPU-ms/request, execution adds about $0.40. Reads
fit the 25-billion-row allowance and fixture receipts occupy well below 5 GB.
Other projects share these allowances. This is a planning model, not a bill or
an all-in price guarantee; retries, enrollment, abuse and existing account usage
must be included in production estimates.

Sources: [Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/),
[D1 pricing](https://developers.cloudflare.com/d1/platform/pricing/), and
[edge limiter accuracy](https://developers.cloudflare.com/workers/runtime-apis/bindings/rate-limit/#accuracy).

For an existing deployment, apply the migration and deploy the new Worker with
the rate-limit binding in one maintenance window. Keep registration/delivery
traffic quiescent during the migration/cutover so the old Worker cannot recreate
bucket-suffixed counters. The iOS/Mac HTTP contract is unchanged.

## Verify

Mocks prove authentication, deduplication and stale-token behavior; they do not
prove APNs delivery. Confirm actual foreground enrollment and alerts while the
app is backgrounded and terminated, denied permission, token rotation, revocation,
and the correct parent conversation after a Tailscale reconnect. Keep sandbox
and production results separate. `/healthz` reports notification configuration and optional remote-access capacity.

## Optional remote access

The Worker also owns the control plane for **Reach from anywhere**, an opt-in
feature for signed-in phones. Pairing, the home network and Tailscale remain
account-free. No app session, pairing check or device signature is replaced by
this feature. Any signed-in account can use it when `RELAY_ALLOWED_ACCOUNT_IDS`
is `*`. Cloudflare ends TLS at its tunnel edge, but only the sealed RPC path
(`/api/v1/rpc/sealed`) crosses the tunnel and the edge sees only ciphertext plus
traffic metadata (`crates/wonderd/RELAY.md`). Screen video
must use the home network, Tailscale or direct WebRTC with free STUN; there is no
TURN or video proxy in this Worker.

Relay routes return `503 relay_not_configured` unless the deployment supplies
the Cloudflare bindings and an allowlist value:

- `CF_TUNNEL_TOKEN`: a separate **Worker-held secret**, scoped to Cloudflare Tunnel
  Edit on the account and DNS Edit on this zone. Never use the host/operator's
  `WONDER_CF_RELAY_TOKEN` environment variable in Worker code or client requests.
- `CF_ACCOUNT_ID` and `CF_ZONE_ID`: 32-character Cloudflare IDs.
- `RELAY_ALLOWED_ACCOUNT_IDS`: `*` allows any signed-in account; otherwise a
  comma-separated list of approved account IDs. Missing or empty denies all
  grants/approvals (`403 relay_not_available`). Per-account and per-IP rate
  limits, the five-hosts-per-account limit, capacity (`relay_full`) and the
  reaper apply either way.
- `RELAY_ZONE`: `getcupola.app` for the approved zone. There are no API-base,
  upstream-service or hostname overrides in requests.

The permanent address is `https://r-<label>.<RELAY_ZONE>`: first-level DNS names
covered by free Universal SSL. Labels contain 128 random bits in 26 base32
characters. No computer/user names enter them. Tunnels use deterministic
`wonder-<label>` names, remotely managed ingress to `http://127.0.0.1:3777` for the path
`/api/v1/rpc/sealed` only (the edge answers 404 to anything else; see
[crates/wonderd/RELAY.md](../../crates/wonderd/RELAY.md)), and
a final `http_status:404` rule. Only that reserved hostname's CNAME is changed.
The tunnel runtime token is returned only to the proven host, never stored in D1
or returned to the approving phone.

### Changing the relay zone

Confirm the operator token can read the target zone and edit its DNS before
changing anything. Inventory both zones, the Worker's custom domains/routes and
`relay_hosts` (including granted and deleting rows). An empty host table permits
a direct switch; existing hosts require a separate migration so cleanup never
looks for their old DNS IDs in the new zone. Preserve every record outside the
reserved `r-*` namespace and every unrelated tunnel.

Update the custom domain in `wrangler.jsonc`, commit, then run
`scripts/deploy-push --dry-run`. Supply the new `CF_ZONE_ID` and `RELAY_ZONE` with
`wrangler secret put` through stdin, then deploy with `scripts/deploy-push`.
Keep `CF_ACCOUNT_ID`, `CF_TUNNEL_TOKEN`, the owner allowlist and all push secrets.
Secret updates take effect immediately, so perform this switch only while no
relay hosts exist. Expire the `relay_capacity` snapshot and refresh it through
the signed-in capacity endpoint so health reports the new zone's DNS count.

Compare `/healthz`, `/privacy` and push/account error contracts on the existing
and new Worker origins. Verify an owner grant and signed host connect create
exactly one first-level `r-<label>` CNAME in the new zone, then use signed disable
and confirm both tunnel and CNAME are gone. Record the prior Worker version and
zone bindings for rollback; rolling back code does not remove a custom domain
or migrate live relay resources.

### Phone and host contract (D4 / I4)

All POST requests use JSON. Phone operations need the existing Bearer account
access token. Host operations use raw P-256 X9.63 public keys (87 base64url
characters) and raw `r || s` ECDSA/SHA-256 signatures, like the existing account
proofs. Host proofs have `{ts, nonce, sig}`: Unix seconds within 60 seconds,
a fresh 32-byte base64url nonce (43 characters), and the signature. A nonce is
single-use across actions for that host key; retry with a fresh proof. Transcript
strings have no trailing newline:

| Operation | Request and response | Signed transcript |
| --- | --- | --- |
| Grant from a signed-in phone | `POST /v1/relay/grants {hostKey}` → `{grant,label,expiresIn:600}` | Existing account authentication; hand off with daemon's signed `relay.enable` action |
| Connect/reconnect from the host | `POST /v1/relay/hosts/<label>/connect {grant?,ts,nonce,sig}` → `{token,origin,label}` | `relay-connect\|<label>\|<ts>\|<nonce>` |
| Disable from the host | `POST /v1/relay/hosts/<label>/disable {ts,nonce,sig}` → `{disabled:true}` | `relay-disable\|<label>\|<ts>\|<nonce>` |
| Begin QR approval from the host | `POST /v1/relay/pending {hostKey,ts,nonce,sig}` → HTTP 202 `{id,secret,expiresIn:600}` | `relay-pending\|<hostKey>\|<ts>\|<nonce>` |
| Approve from a signed-in phone | `POST /v1/relay/pending/<id>/approve {secret}` → `{approved:true,label,origin}` | Existing account authentication plus QR secret |
| Poll from the host | `GET /v1/relay/pending/<id>?ts=...&nonce=...&sig=...` → HTTP 202 `{approved:false}` or HTTP 200 `{approved:true,token,origin,label}` | `relay-poll\|<id>\|<ts>\|<nonce>` |
| Operator capacity | `GET /v1/relay/capacity` → `{used,limit,state,dns,budget}` | Signed-in; served from the shared snapshot |

The grant is bound to the exact host public key and account, expires after ten
minutes, and is consumed once in the same transaction that enables the host.
Invalid signatures do not consume it. Once redeemed, retry without the grant;
the host key authorizes later reconnects. `410 relay_gone` means turn remote
access off until enabled by a new grant. `503 relay_full` includes
`Retry-After: 600`; other temporary failures include `Retry-After: 60`.
`403 replayed_proof` means generate a fresh nonce/signature. A concurrent control
operation can return `503 relay_busy`; retry with a fresh proof. Do not log grant,
QR secret, signatures or runtime tokens.

Pending requests make **no Cloudflare API calls**, DNS records or tunnels before
signed-in approval. The QR carries the pending secret and the host fingerprint,
plus the existing pairing offer. Approval creates the address but does not pair a
device: ordinary host-confirmed pairing and pinned host proof still apply.
Pending polling/approval expires ten minutes after creation. Retrying approval
from the same account is idempotent; a different account cannot take it over.

At most five reserved/enabled hosts belong to an account, including offline hosts
whose addresses are retained. Repeated grants for one key reuse its label. A new
grant replaces any outstanding grant for that label. `409 relay_host_limit`
means disable an unused host. An unredeemed reservation is reclaimed after ten
minutes once no valid grant remains. A key cannot be claimed by another account.

### Reaping, recovery and budget

The five-minute cron reads the dedicated Cloudflare `/connections` endpoint,
including each connector's nested `conns` array. The first cron that finds no
connection records `offline_since`; ten minutes after that (10-15 minutes after
the connector dropped, never sooner) the cron deletes only the tunnel. A signed
connect that fetches a token restarts that clock. The host row, label and CNAME remain. A later
signed connect recreates the tunnel and updates the same DNS record. An unknown
or failed liveness response never counts as offline. The existing receipt scan
and expired-session sweep still run hourly (`17 * * * *`), avoiding a twelvefold
increase in receipt scans.

Sign-out disables **all remote access for that account**, while notification
revocation remains per phone. Account deletion also revokes that account's remote
access. Session replacement (including signing in again on the same phone) and
expiry do not disable enabled hosts: they belong to the account, independent of
the approving phone's session. Unredeemed grants still require their issuing
session. Only explicit sign-out, account deletion or a signed host disable revoke
enabled hosts. Sign-out/deletion cleanup is scoped to the acting account; sign-in
does not wait on Cloudflare cleanup. The credential transaction
marks host rows for deletion and invalidates outstanding grants/pending approvals.
Healthy cleanup first clears all active tunnel connections with Cloudflare's
connections DELETE, then deletes the tunnel, then its CNAME and metadata. Each
step must succeed (or already be absent) before the next step runs. Sign-out and
deletion respond as soon as credentials are revoked, with `cleanupPending:true`
when the account had remote access, and run the purge in the background
(`waitUntil`). Durable `tombstone` rows survive account/session deletion and are
the purge queue: a failed purge is retried by cron after 4, 8, 16 and 32 minutes,
then hourly, due rows first, each attempt with the same bounded API backoff. Disable failures also retain the
tombstone and return a temporary error. If clearing connections fails, a running
connector can remain reachable until Cloudflare recovers; reconnecting through
the Worker is refused immediately. Account and relay
cleanup never weakens the daemon's independent device/session checks.

A shared, renewable D1 lease serializes provisioning and deletion across Worker
isolates; requests that lose it stop before the next API call. Safe API methods
retry transient errors up to three attempts with jittered exponential backoff
and bounded `Retry-After`. Create POSTs are not blindly retried: the deterministic
tunnel name is looked up first after an uncertain result. Partial operations keep
resource IDs in D1, or recover DNS by its exact random name. Replaced/unrelated
DNS records are preserved and reported as a conflict.

Admission reads fresh Cloudflare account-wide tunnel and zone-wide DNS totals,
including unrelated resources. It refuses creation at **1,000 tunnels** or
**200 DNS records**, keeping the approved free limits. Existing tunnels continue
at capacity; recreation can reuse its retained DNS slot. There is no paid-capacity
purchase API or configurable increase beyond the free caps. Serialize other
operators' provisioning too: the D1 lease coordinates this Worker, not independent
Cloudflare administrators.

`/healthz.relay` and `/v1/relay/capacity` report `ok`, `near` (80%) or `full`, with
`budget.spendAlert` for operator inspection. Both read one D1 snapshot that the
hourly cron and every live admission count refresh; health never calls
Cloudflare and reports `unknown` with an alert once the snapshot is over 65
minutes old. A signed-in capacity read refreshes a stale snapshot; admission
always recounts live. The budget object records the owner's $5/month
target and $25 hard ceiling; **it is not a measured bill or a billing cutoff**.
The account-wide spend alert/limit must be configured and verified separately at
the provider before deployment. No alert messages are sent by this unit.

### Local verification

`npm run check` and `npm test` use workerd/Miniflare D1 migrations and a strict
mock of `https://api.cloudflare.com/client/v4`. The owning relay test covers the
W1 lifecycle, idle boundary, capacity, pending approval, signatures, nonce replay,
five-host limit, account revocation, uncertain mutation recovery, API failures and
concurrent admission. It does not create real tunnels, contact APNs, deploy the
Worker, or verify real hostname recovery/latency. Those remain separate gates.
API shapes were checked against Cloudflare's [tunnel list](https://developers.cloudflare.com/api/resources/zero_trust/subresources/tunnels/subresources/cloudflared/methods/list/),
[connector list](https://developers.cloudflare.com/api/resources/zero_trust/subresources/tunnels/subresources/cloudflared/subresources/connections/methods/get/)
and [connector cleanup](https://developers.cloudflare.com/api/resources/zero_trust/subresources/tunnels/subresources/cloudflared/subresources/connections/methods/delete/),
the [tunnel deletion prerequisite](https://developers.cloudflare.com/api/resources/zero_trust/subresources/tunnels/subresources/cloudflared/methods/delete/),
and [DNS list](https://developers.cloudflare.com/api/resources/dns/subresources/records/methods/list/)
documentation. Address reuse follows [T3 remote access](https://raw.githubusercontent.com/pingdotgg/t3code/main/docs/user/remote-access.md).
