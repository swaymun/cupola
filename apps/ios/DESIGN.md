# Cupola design language

The owner's taste, written down so workers and reviewers apply it before the
owner has to. Judge every screen as a person using it on an iPhone, iPad and
Surface Duo (folded and unfolded), not as a test that found the right element.

## Principles

- **Every control earns its space.** One way to do each thing per screen. A
  button that repeats what a pill, toggle, swipe or another button already does
  is clutter, especially on iPhone and the folded Duo.
- **Dismiss the native way.** A pane opened by a pill or toggle closes with
  that same pill; sheets and pop-ups close by swiping down. Add Done only when
  there is no other way out, and never one that doesn't fit on screen.
- **Mode controls sit where Done would be.** A pane with half- and
  full-screen modes puts its expand/collapse control in that top corner.
- **No headers that repeat the toggle.** If the Files pill is on, the pane
  doesn't need a "Files" title bar, and the same goes for a workspace row that
  restates what is already visible. The Duo's Chats panel needs no "Chats"
  header.
- **Controls go where people expect them.** Close and dismiss follow iOS
  placement; primary actions sit on the trailing side or at the bottom within
  reach; the same control looks and sits the same on every platform.
- **Use the whole screen.** On the Duo and iPad, content fills both panes and
  lines up across the hinge; status like "Mac dialog waiting" or which
  computer is shown takes the least space that still reads clearly (a badge or
  a title, not a banner).
- **Aligned and calm.** Shared edges, consistent spacing and row heights,
  restrained corners and few borders (see AGENTS.md "Product judgment").
- **Menus make sense.** Items in a ⋯ menu belong to that screen, are grouped,
  read in plain language, and don't duplicate a visible button. Prefer a flat
  menu over a submenu with one or two items.
- **Keep what works.** Queued messages as one quiet row under the composer is
  the pattern the owner likes; extend it rather than move it into the chat.

## Agents, screen sharing and devices

- **One status vocabulary everywhere.** Working, needs you, done, failed: the
  same words, icon and color in Chats, the header, the Duo rail,
  notifications and Live Activities. Never color alone.
- **"Needs you" always wins.** Approvals, questions and waiting Mac dialogs
  share one badge style, sit nearest the composer or rail, and jump straight
  to the item when tapped.
- **Answer first, work folded.** The final reply is prominent; tool calls and
  thinking fold into one quiet "Working" row that expands on tap.
- **Undo over confirmation.** Send, delete, stop and archive act at once with
  a short undo; confirm only what cannot be undone.
- **The shared screen is the content.** The remote display fills every
  available pixel; its controls float as translucent pills over it. The header
  buttons (Close, the computer picker, More, window size) stay put and never
  fade or hide on idle, so the first tap always lands on the control; below the
  picture is one control row (who controls, keyboard, trackpad, dictation, the
  Mac's dock), not a stack of bars. No bar background or hairline.
- **Who is in control is unmistakable.** You, the agent or nobody: a border
  color plus a short label, always in the same spot; take/give control is one
  tap in one fixed place.
- **Honest connection without banners.** When the link stalls, dim the last
  frame and show its age ("2 s ago") with a small quality dot.
- **Which computer, compactly.** Name and a small machine glyph in the title;
  switching lives in that title's menu, not a separate row.
- **Duo: big chat when alone, one task per screen otherwise.** With Chats
  closed and no tool open, the chat spans both screens. With a tool open,
  chat on one side and the tool on the other. Controls (buttons, menus,
  the scroll arrow) never sit in the hinge gap. One exception: on a
  foldable with no visible gap, the pane-swap control sits centered on the
  divider at header height. Folded is half an iPad
  mini, not a narrow iPhone: one column beside the system rail, composer
  close to the bottom edge, scroll arrow centered above the composer,
  and header edges that end cleanly at the rail.
- **iPad: persistent sidebar, keyboard and pointer.** Shortcuts for send,
  switching chats and toggling panes; hover states on controls.
- **iPhone: thumb reach.** Frequent actions at the bottom; the top is for
  reading.
- **Presence and handoff.** Show subtly which of your devices is viewing or
  controlling a chat or screen, with one-tap "continue here".
- **One identity per agent, provider and computer.** The same avatar and
  accent everywhere, including notifications.
- **Type and motion.** Two or three text sizes per screen, monospace only for
  code and terminal; panes slide from the control that opened them, subtly,
  respecting Reduce Motion.
- **Minimal empty and loading states.** Skeletons instead of spinners with
  sentences; an empty state is one line and one action.

## Duo feedback 2026-11-02

From the owner's folded and unfolded Duo recordings
(plans/launch-feedback/duo-feedback-2026-11-02.html; issue numbers in brackets).

- **Icons without captions.** Composer, rail and computer shortcut buttons
  are icon-only; names live in accessibility labels and long-press (1, 27).
- **No header bar.** The conversation title floats on the page color with a
  scroll-edge fade only when content is beneath; unfolded and iPad portrait
  keep the header to one row (2, 4).
- **Floating chrome.** The title is a pill floating over the timeline (messages
  scroll beneath it, no band above them), centered on its display and never on
  the hinge; the scroll-to-end control is a centered icon with no fill or
  shadow, sitting below the last visible line so it never covers the chat.
  The title menu's first row is the whole title and copies it; commands in the
  timeline stay one line with an ellipsis and copy whole on long-press.
- **Offline is a red dot.** No "Try again", "Retrying…" or "couldn't check"
  rows; reconnects retry silently, and unreachable hosts are simply absent
  from tool lists (11, 19).
- **One picker row shape.** Icon, name, status dot after the name (green or
  red), trailing check (12, 18).
- **One rail icon per tool.** Computer, Simulators, Files, Terminal and
  Browser each have their own icon and close with it; no Back plus Close (19).
- **Strips scroll along their own axis.** The folded computer view is
  portrait-locked, and the Mac dock is a horizontal strip above the shortcut
  row, never in the vertical rail (21, 22).
- **Keys release.** Modifier and switcher keys release on the next tap;
  double-tap latches with a visible underline. No unexplained keys (24, 25).
- **Popovers match their surface** and never cover Search (8).
- **Swipe to delete** what the user owns, with one plain consequence
  sentence (9).
- **Paths and technical values.** Abbreviate the home folder to ~; routes,
  keys and paths copy on long-press (14, 31).
- **Files are always named:** relative path plus a language badge, never a
  placeholder (30).
- **Links open in the in-Cupola viewer** with its ✕ rail, not a Close sheet
  (32).
- **Never refuse a message.** While a reply runs, sending steers it or queues
  it in the quiet queued row; no "can't take a message" text (5).

## Review checklist

For each changed screen, on iPhone, iPad, Duo folded and Duo unfolded, light
and dark (before and after):

1. Is any control redundant with a pill, toggle, swipe or another button?
2. Is there a Done button that swipe-down or the opening pill already covers?
3. Does any header or row only repeat what the toggle or title already says?
4. Is every control where an iOS user would reach for it, and the same on every platform?
5. Is anything clipped, off-screen, overlapping or wrapping badly (including large text)?
6. Is space wasted: banners, empty bars, padding, a pane narrower than it could be?
7. Do edges, spacing and row heights line up, including across the Duo hinge?
8. Do ⋯ menus contain only relevant, grouped, plainly worded items?
9. Is the status shown (which computer, who controls, waiting dialogs, connection) compact, consistent and clear?
10. Would a first-time user understand what each control does without a label explaining it?

11. Are setup choices pills in one row (at most two on a phone at ordinary text sizes), with no redundant New chat heading? (3938 [00:00], 4849 [10:04]–[10:31].)
12. Have helper paragraphs, offline retry rows, size labels and repeated titles been removed where they add no useful action? (3593 [02:09], [10:47]–[10:58].)
13. Does Chats end just past Search without an artificial hinge band; do landscape tools align with the composer, and do tall windows use a dismissible half/full sheet? (3593 [01:46]–[01:56], [03:19]–[03:59].)
14. Are private connection details masked until tapped, with owned relay addresses omitted? (4849 [00:00]–[00:37].)

A unit with a "yes" to 1–3 or 5–7, or a "no" to 4, 8–14, is not done.

### Voice target chip (voice tools)

A spoken "open X" is the only thing that moves voice. Check on every device:

- The chip or panel line reads "Now talking in <name>" for a few seconds after a spoken move, then "Talking in <name>"; VoiceOver announces the move.
- Opening a screen or pane by voice leaves the chip on the old target, and the tapped-away screen never changes it.
- Start, stop and approve show the confirmation card (summary and full text) and no new control; the card is not clipped at large text.
