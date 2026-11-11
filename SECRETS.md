# Project secrets

An agent asks for a named secret with `wonder_request_secret({name, purpose})`.
Cupola shows the purpose in the chat's request view. The owner types into a
secure field and chooses Save, then confirms with Face ID, Touch ID or, when
biometrics are unavailable or locked out, the device passcode. Cancel
requires no device check. A failed or cancelled check sends no
value; the field clears on submission, backgrounding and dismissal. There is
no secret draft, keyboard prediction, notification reply or voice readout.

The signed `POST /api/v1/secret-requests/{approvalId}` is separate from durable
approval replies. Its signature binds the value, decision, nonce, pending state,
idempotency key, device, host and issue time. Expired, replayed, moved-Project and
archived-chat requests are refused. The value never becomes a saved approval
answer. On network failure the owner retypes it; Cupola does not persist a retry.

## Storage and grants

The Mac uses Security.framework directly, targeting the login Keychain. Each
Project has a service derived from the host installation ID and stable Project
ID, rather than its editable name. The account is the secret's name. The item's
protected payload contains the value, a random `secretRef`, and the identities
of chats the owner approved. None of these values is put in SQLite, JSON files,
logs, diagnostics, push messages or history. Only the names and grant metadata
leave the vault in the inventory API. Test builds use a fixture service derived
from each test's isolated data directory and delete their items even on panic.

A reference works in later turns of its approved chat. Other Projects and chats
cannot use a stolen reference. Requesting an existing name in an approved chat
returns its ref; another chat requires a new owner Save. Saving a new value for
that name replaces the value and preserves the previously approved chats.
Changing a chat's provider retains that chat's grant; the inventory records the
provider that requested each grant. The host rechecks Project/chat ownership
and permissions on use. The old `env_file` destination is no longer accepted.
Existing legacy items are neither read nor migrated automatically; old pending
requests must be asked again with the new schema.

## Command delivery

Both Codex dynamic tools and the Claude MCP bridge receive the same registered
`wonder_run_with_secret` tool. Example (the reference is illustrative):

```json
{"secretRef":"secretRef:opaque-reference","delivery":"file","variable":"TOKEN_FILE","command":"wc -c < \"$TOKEN_FILE\"","timeoutSeconds":30}
```

The tool requires the chat's existing **Full access** permission. It does not
widen Workspace or Read only permissions or run through an unrelated external
MCP grant. Commands run in the chat's folder with a minimal environment; the
specified variable contains either the value (`env`) or a private 0600 file path
(`file`). File delivery is preferred. Each call gets its own folder (0700) holding the file (0600),
inside Cupola's `secret-delivery` folder in the data directory. The host creates
that folder 0700, tightens an older looser one, and refuses one that is a link or
owned by another user. It also refuses to deliver when that folder sits inside
the chat's folder or a shared temporary folder (`/tmp`, `/var/tmp`, the
system temp dir), where an agent's sandbox could read it. The private folder,
file and process group belong to the host call. Completion, failure, timeout,
turn cancellation and future cancellation clean them up; descendants are killed
before the file is removed. Commands cannot run detached. Runtime is at most
120 seconds, with 1 MiB per output stream. An output-limit error returns no
captured output. Values are never command-line arguments or tool results.

Full access commands are trusted code. They can deliberately copy or encode a
value, send it over the network, or read the login Keychain directly. The vault
prevents routine transcript disclosure; it is not a sandbox against malicious
code running as the owner's Mac account. Only the host-owned delivery tool can
redact output before a provider receives it. Native provider tools bypassing
this delivery can already have sent their output to that provider; Cupola cannot
retroactively erase the provider's own logs or history. Encoded/hashed/transformed
secrets are not claimed to be detected. A forced host kill can interrupt immediate cleanup. On restart Cupola removes
only this installation’s abandoned delivery directories, checking the creating
host PID so a running host’s files are left alone.

## Redaction and management

The host replaces exact occurrences with `[secret:NAME]`, longest match first,
before returning a command result, persisting inbound notifications, or rendering
native history. Matching precedes truncation. Streamed deltas hold a suffix that
could start a saved value until it can be classified; independent tools have
independent buffers. Redactor state commits only after durable ingestion succeeds.
Protocol routing metadata remains intact. The host logger also redacts known
values. Values are loaded from this installation's Project services before
providers start. A Keychain that cannot be read at startup (locked, SSH session,
access prompt after a rebuild) does not stop the host: it logs that the vault
will retry and retries every 30 seconds, and each vault read also loads the
values it sees. Until a retry succeeds, values saved earlier are not yet
redacted in new history. Old values remain in
memory after rotation or deletion until process exit to protect late output.

Settings → Secrets lists names, Projects and the requesting chats/providers.
Swipe to delete and confirm the consequence. The authenticated owner-only
`GET /api/v1/secrets` has no value field or reveal operation. Its `DELETE` accepts
only Project ID and ref; after success new deliveries fail. Already-running
commands may have consumed the value. Deletion is irreversible.

## Evidence

Owning checks are `cargo test -p wonderd secret --lib` (its delivery tests pin the 0700 root, per-run folder and 0600 file modes, the link and readable-folder refusals, and startup cleanup), the phone approval tests,
`swift test --package-path apps/native --filter 'SecretsTests|AttentionTests'`,
`vault-secrets.wctl`, the four-device preview recipe and the visual gate.
The Rust fixture uses real fixture-service Keychain items and scans SQLite/WAL
and host logs for its generated value. It checks file permissions, length-only
output, output redaction, permission/ref scope, replay and deletion. Offline UI
fixtures exercise the production request transport and list/delete controls;
they do not prove a real Mac Keychain or real biometric hardware. The live check
and simulator/physical limits belong in the unit report, not in a build verdict.
