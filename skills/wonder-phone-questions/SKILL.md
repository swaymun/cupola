---
name: wonder-phone-questions
description: Use when the user's message says it came from their phone through Wonder and you need them to choose or confirm something before you can go on. Asks in a form Wonder shows as buttons they can tap, even on the phone's notification, instead of a question buried in a long reply.
---

# Asking someone on their phone

The user is away from this computer. A question written at the end of a
long reply reaches them as a "finished" notification showing only the start
of that reply; they have to open the chat, find the question and type an
answer on a small keyboard.

Ask only for a choice that is theirs and costly to undo; make cheap,
reversible choices yourself and say how to change them. Do the work that
does not depend on the answer first, then ask.

## Ask with your question tool

If you have a tool for asking the user a question with choices (Claude
Code's `AskUserQuestion`, Codex's `request_user_input`), use it rather than
writing the question in your reply. Wonder shows its choices as buttons in
the chat, and on the notification itself when the question fits these
limits:

- One question with one answer, not several questions or a multi-select.
- The question in one sentence of at most 300 bytes, saying what each
  choice leads to. A header, if any, of a few words (at most 80 bytes).
- Two to four choices with short, different labels (at most 120 bytes
  each). Add a description (at most 160 bytes) only when the label alone
  is unclear.
- Header, question and every choice with its description together within
  382 bytes, so older phones can show them all. Limits are UTF-8 bytes:
  accented letters, symbols and emoji count two to four each.
- Your recommendation first, with "(recommended)" in its label.

Leave out an "Other" choice: Wonder lets them type their own answer. If you
have several questions, ask the one that blocks you and decide the rest
yourself, saying what you chose.

## Without a question tool

Put the question at the very start of your final message, so the
notification shows it, as a numbered list of choices with your
recommendation first, and say they can reply with the number.
