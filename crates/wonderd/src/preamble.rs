//! Cupola's preamble: the few sentences every agent Cupola starts gets about
//! where it runs (Cupola, a messenger the owner often uses from the phone)
//! and, while the thread tools are on, how to reach the Project's other
//! threads. This module owns the text; each provider gets it through the one
//! channel its harness has:
//!
//! - Codex: an application entry in every Cupola turn's `additionalContext`,
//!   beside the phone signal and the owner's instruction. Codex's
//!   `developerInstructions` would replace the owner's own
//!   `developer_instructions`, so Cupola leaves them alone.
//! - Claude: `wonderPreamble`, which the bridge puts first in what it appends
//!   to Claude Code's system prompt.
//! - OpenCode and Cursor: ACP has no system prompt, so a new session's first
//!   prompt opens with it as a briefing block the history views hide.
//! - T3 Code: its launch takes only the first message, so a thread Cupola
//!   starts there opens with a shorter briefing block (T3 runs its own tools,
//!   not Cupola's). T3 Code shows that block in its own window.
use crate::provider_switch::{HANDOFF_CLOSE, HANDOFF_TAG};

/// The `additionalContext` key of the preamble on Codex turns.
pub(crate) const CONTEXT_KEY: &str = "wonder_preamble";

const ABOUT: &str = "This conversation runs in Cupola, the user's messenger for their agent conversations. They often read and reply on their phone, away from this computer's screen: keep replies readable on a small screen, and say plainly when something needs them at the computer.";

const THREAD_TOOLS: &str = "Cupola's thread tools reach the other threads of this Project: wonder_thread_list, wonder_thread_read and wonder_thread_send to see and message them, wonder_delegate to give a self-contained task to a new thread on any provider (it sees only your prompt), and wonder_thread_wait for a thread's answer. wonder_show_visual shows the reader a table, chart, plan or mockup drawn natively on their phone.";

/// The preamble for a Cupola-run session, naming the thread tools only when
/// the session is offered them.
pub(crate) fn text(thread_tools: bool) -> String {
    if thread_tools {
        format!("{ABOUT}\n\n{THREAD_TOOLS}")
    } else {
        ABOUT.to_owned()
    }
}

/// The preamble as the opening part of a session's first prompt, for a
/// harness without a system prompt. It is a briefing block, so Cupola's
/// history, titles and thread reads never show it as the owner's words.
pub(crate) fn first_prompt_block(thread_tools: bool) -> String {
    block(&text(thread_tools))
}

/// The first message of a thread Cupola starts in T3 Code: the briefing,
/// then the owner's message. T3 Code offers its own tools there, so the
/// preamble does not name Cupola's. A native slash command must remain the
/// leading input: a briefing before /compact turns it into an ordinary prompt.
/// Such a thread starts without the briefing and the first later message
/// that is not a command carries it (`projects::t3_briefing_owed`); a path or
/// other text that only begins with `/` still gets it at once.
pub(crate) fn t3_first_message(message: &str) -> String {
    if leads_with_command(message) {
        return message.to_owned();
    }
    format!("{}{message}", block(ABOUT))
}

/// `/name` or `/ns:name`, then a space or the end: not `/Users/me/file`.
fn leads_with_command(message: &str) -> bool {
    let Some(rest) = message.trim_start().strip_prefix('/') else {
        return false;
    };
    let name = rest.split(char::is_whitespace).next().unwrap_or("");
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

fn block(text: &str) -> String {
    format!("{HANDOFF_TAG} purpose=\"cupola\">\n{text}\n{HANDOFF_CLOSE}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_switch::{is_handoff_text, without_handoff};

    // Contract: the tool sentence follows the session's thread tools, and
    // the block forms open as a briefing that leaves exactly the owner's text.
    #[test]
    fn preamble_names_tools_only_when_offered_and_hides_as_a_briefing() {
        assert!(text(true).contains("wonder_delegate"));
        assert!(!text(false).contains("wonder_"));
        assert!(text(false).contains("phone"));
        for tools in [true, false] {
            let block = first_prompt_block(tools);
            assert!(is_handoff_text(&block));
            assert_eq!(without_handoff(&block).trim(), "");
        }
        let first = t3_first_message("Fix the parser");
        assert!(is_handoff_text(&first));
        assert!(!first.contains("wonder_"));
        assert_eq!(without_handoff(&first).trim_start(), "Fix the parser");
        for command in [
            "/compact",
            "/code-review 17",
            "/plugin:inspect @\"docs/Release notes.md\"",
        ] {
            assert_eq!(t3_first_message(command), command);
        }
        let path = t3_first_message("/Users/me/app/main.rs fails to build");
        assert!(is_handoff_text(&path));
        assert!(path.ends_with("/Users/me/app/main.rs fails to build"));
    }

    // Contract: Cupola's history shows a session's first message without the
    // preamble: a separate part (ACP) goes, and one that opens the owner's
    // text (T3 Code) keeps exactly that text.
    #[test]
    fn history_hides_the_preamble_but_keeps_the_owners_words() {
        let item = crate::history::sanitize_typed_item(
            &serde_json::json!({
                "type": "userMessage", "id": "u1",
                "content": [
                    {"type": "text", "text": first_prompt_block(true)},
                    {"type": "text", "text": "List the threads"},
                    {"type": "text", "text": t3_first_message("Fix the parser")},
                ]
            }),
            None,
        );
        assert_eq!(
            item["content"],
            serde_json::json!([
                {"type": "text", "text": "List the threads"},
                {"type": "text", "text": "Fix the parser"},
            ])
        );
    }
}
