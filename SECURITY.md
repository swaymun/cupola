# Security

## Reporting a vulnerability

Please use GitHub's **Report a vulnerability** (private security advisory) on
[swaymun/cupola](https://github.com/swaymun/cupola/security/advisories/new). If that is
unavailable, open an issue that asks for a private channel and contains no exploit
details or private data.

Do not publish credentials, pairing links, APNs tokens, message bodies, screenshots
or user database files in issues or pull requests.

Include affected versions, the trust boundary involved, a minimal reproduction with
synthetic data, and the impact. Cupola is in beta: there is no promised response time,
and only the latest release and `main` are supported.

## Trust model

Tailnet membership is transport access, not Cupola authorization. Mac-confirmed
pairing, device signatures, session renewal, CSRF protection, approval policies and
revocation remain required. Screen control additionally requires native permissions.

The push Worker has only APNs authority. It cannot fetch a Mac's conversations or
files. APNs keys belong in Worker secrets, and phone-specific sender capabilities
stay on the paired Mac and in the phone Keychain. Revocation cancels local pending
deliveries and queues provider revocation. A notification already handed to APNs
cannot be recalled. Retire an independently deployed Worker/key if all of its
clients are removed.

Release builds exclude Cupola's diagnostics recorder, test fixtures and diagnostic
UI. Do not attach Diagnostics archives to issues.

## Secrets an agent asks for

An agent that needs a token calls `wonder_request_secret` with a name and a purpose.
The paired phone shows a secure field; Save (confirmed with Face ID, Touch ID or the
device passcode) sends the value, signed, straight to the Mac, which writes it to the
login Keychain in a per-Project vault service. The agent's result carries only the
state and an opaque `secretRef`; it uses the value through the host-owned
`wonder_run_with_secret` tool, which delivers it to a command as an environment
variable or a private temporary file and redacts exact occurrences in the output. The
value is never in the request's saved answer, events, push previews, transcripts,
history or logs, and the phone does not draft or save it. Commands run with Full
access are trusted code and can still read the value back; this protects transcripts
and provider logs, not the Mac from its own agent. See [SECRETS.md](SECRETS.md).
