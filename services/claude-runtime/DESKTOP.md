# Claude Desktop research — deferred

Decision, 2026-09-29: remove the Desktop experiment. Claude Project threads use the
**Agent SDK with Cua Driver when it is installed and enabled on the host**. There is no
Desktop runner, setting, session-creation command, or automatic Desktop fallback.
Cupola's existing access policy applies to the driver's tools.

The removed bootstrap is recoverable from commit `536c28af`; its rollout notes
were in `302b0e30`. These were experimental source, not an accepted product mode.
Keep this research separate from the shipped SDK implementation.

## Verified observations

Tests used Claude Desktop 2.9939.4 and its bundled CLI 2.1.284 on macOS. These
observations are version-specific, not a stable integration contract.

- The documented `claude://code/new?folder=…&q=…` link opens a draft and requires
  native workspace confirmation. It does not send the prompt. The research computer-use
  UI actions created a Sonnet 5.5 session; owned metadata, process identity,
  transcript ancestry, exact reply, stop hook, and idle state verified completion.
- The folder name shown by Desktop was insufficient evidence: one session used
  a scratch directory despite displaying the requested folder. Explicitly
  selecting the exact full path in the folder menu resolved that observed case.
- The documented local inbox accepted a no-tool peer message in an owned live
  session. Replies came from its transcript, not a response stream on the socket.
  A receipt written before delivery and read-only observation prevented resending
  during the bounded reconnect check. Peer messages cannot approve permissions,
  change permission settings, or become user authorization. The installed inbox
  preserves a supplied message UUID and adds explicit peer-authority text.
- Native Claude computer use successfully operated Calculator after its normal
  app-access approval. The observed browser control was view-only; interactive
  browser work required Claude in Chrome. Cupola must not depend on that extension.
- The installed app supports `claude://code/continue?session=local_UUID` for an
  existing session. Source inspection and a live route check verified navigation
  to the owned session. This was not a test of Stop, approval recovery, cold
  launch, or worker restart.
- SDK sessions alone did not supply Desktop's native execution host. The local
  inbox is not a generic MCP/control endpoint. The browser SDK accepts Remote
  Control endpoints, but no supported Cupola client enrollment/authentication
  path was established. Do not copy Desktop cookies or first-party credentials.
- Channels can relay some CLI tool approvals, but installing an MCP server does
  not opt a Desktop session into Channels. No verified path connected that relay
  to Desktop's separate native computer-access approval broker.

## Why this is not a product mode

Desktop owns its process, app consent, and filesystem/tool policy. It cannot
inherit Cupola's `ToolPolicy` callbacks or protected-folder restrictions through
an inbox message. A future mode needs enforceable policy equivalence or an
explicit, separate Desktop-managed permission choice; an experimental toggle
must not silently widen a restricted Project thread’s access.

The bootstrap required a cooperative foreground session, a preselected model,
and multiple native computer-access approvals. Guarding an accessibility
snapshot reduces wrong-target clicks but does not make the lifecycle deterministic.
The follow-up worker and Stop adapter were unfinished and removed. Mock tests of
a Stop click did not establish that actual model execution stopped. No Settings
toggle or Desktop-backed production session was shipped.

The earlier **4/10 everyday reliability** rating was an engineering estimate,
not a measured success rate. Successful creation and read-only reconnect do not
establish unattended operation.

## Evidence required before revisiting

Use a disposable owned session and record outcomes separately for:

1. Desktop fully closed; running with no window; minimized; behind another app;
   another session selected; an unsent draft; concurrent user interaction.
   Prove the intended app/window/session is reached without disturbing other work.
2. Locked/sleeping Mac, wake, app/worker restart, daemon restart, lost socket,
   process-ID reuse, sign-out, update, and disconnected phone. Never replay work
   after uncertain delivery or bind a replacement process by ID alone.
3. Create, follow-up, real tool results/images, interruption, and approval
  handoff. Match request and session identities; distinguish queued/held/refused
   delivery, awaiting approval, successful completion, and confirmed cancellation.
   Registry `idle` alone proves neither success nor cancellation.
4. Workspace/model verification, attachments, policy enforcement, and stable
   conversation ownership. Existing SDK chats must never migrate implicitly.
5. A supported lifecycle/control interface with authentication and permission
   handling, or repeatable evidence that an alternative is robust across these
   states. UI-only creation is not sufficient to enable the feature.

Private receipts and source/trace evidence remain under
`.local/claude-desktop-sdk-research-20260929/` (`creator`, `desktop-live`, and
`remote-worker`). They contain local identities and potentially private content;
do not publish them. Existing test sessions were left intact, not adopted into Cupola.

References: [Desktop links](https://support.claude.com/en/articles/14729294-open-claude-desktop-with-a-link),
[SDK sessions](https://code.claude.com/docs/en/agent-sdk/sessions#capture-the-session-id),
[session inbox](https://code.claude.com/docs/en/cross-session-messaging#the-sessions-inbox-socket),
[Remote Control](https://code.claude.com/docs/en/remote-control),
[Channels permissions](https://code.claude.com/docs/en/channels-reference#relay-permission-prompts).
