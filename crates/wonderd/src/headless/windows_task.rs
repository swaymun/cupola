//! Reads the host's Task Scheduler definition (`schtasks /Query /XML`).
//!
//! The XML schema is the same in every Windows language, unlike the
//! `/FO LIST` text whose labels ("Task To Run:") are translated. This file has
//! no Windows API calls so its tests run on every platform.
#![cfg_attr(not(windows), allow(dead_code))]

/// The program and arguments a logon task runs.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TaskAction {
    pub(super) command: String,
    pub(super) arguments: String,
}

impl TaskAction {
    /// The command line as Task Scheduler shows it.
    pub(super) fn command_line(&self) -> String {
        if self.arguments.is_empty() {
            self.command.clone()
        } else {
            format!("{} {}", self.command, self.arguments)
        }
    }
}

/// Text of `schtasks /Query /XML` output: UTF-16 with a byte-order mark, or
/// the console code page (read as UTF-8) when it is redirected.
pub(super) fn decode_output(bytes: &[u8]) -> String {
    let units = |data: &[u8], big: bool| -> String {
        let units: Vec<u16> = data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if big {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from_le_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => units(rest, false),
        [0xFE, 0xFF, rest @ ..] => units(rest, true),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        // No mark, but every other byte zero: UTF-16LE of ASCII-heavy text.
        _ if bytes.len() >= 4 && bytes[1] == 0 && bytes[3] == 0 => units(bytes, false),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The first `<Exec>` action of a task definition, or `None` when it has none.
pub(super) fn parse_task_action(xml: &str) -> Option<TaskAction> {
    let exec = element(xml, "Exec")?;
    Some(TaskAction {
        command: unescape(element(exec, "Command")?.trim()),
        arguments: element(exec, "Arguments")
            .map(|text| unescape(text.trim()))
            .unwrap_or_default(),
    })
}

/// The text between `<name>` (attributes allowed) and `</name>`.
fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(found) = xml[from..].find(&open) {
        let start = from + found + open.len();
        match xml[start..].chars().next()? {
            '>' => {
                let end = xml[start + 1..].find(&format!("</{name}>"))?;
                return Some(&xml[start + 1..start + 1 + end]);
            }
            ' ' | '\t' | '\r' | '\n' => {
                let tag_end = xml[start..].find('>')? + start;
                let end = xml[tag_end + 1..].find(&format!("</{name}>"))?;
                return Some(&xml[tag_end + 1..tag_end + 1 + end]);
            }
            // `<ExecutionTimeLimit>` is not `<Exec>`.
            _ => from = start,
        }
    }
    None
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Settings><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions Context="Author"><Exec><Command>C:\Users\A &amp; B\cupola.exe</Command><Arguments>start --data-dir &quot;C:\Users\A\AppData\Local\Cupola&quot;</Arguments></Exec></Actions>
</Task>"#;

    #[test]
    fn the_action_is_read_from_the_xml_in_any_language() {
        let action = parse_task_action(TASK).unwrap();
        assert_eq!(action.command, r"C:\Users\A & B\cupola.exe");
        assert_eq!(
            action.arguments,
            r#"start --data-dir "C:\Users\A\AppData\Local\Cupola""#
        );
        assert_eq!(
            action.command_line(),
            r#"C:\Users\A & B\cupola.exe start --data-dir "C:\Users\A\AppData\Local\Cupola""#
        );
    }

    #[test]
    fn a_task_without_an_exec_action_or_arguments() {
        assert_eq!(parse_task_action("<Task><Actions/></Task>"), None);
        let bare = parse_task_action("<Actions><Exec><Command>a.exe</Command></Exec></Actions>");
        assert_eq!(bare.unwrap().command_line(), "a.exe");
        // `ExecutionTimeLimit` must not be mistaken for `Exec`.
        assert_eq!(
            parse_task_action("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            None
        );
    }

    #[test]
    fn output_is_decoded_from_utf16_with_or_without_a_mark_and_from_utf8() {
        let utf16: Vec<u8> = TASK.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut marked = vec![0xFF, 0xFE];
        marked.extend(&utf16);
        for bytes in [marked.as_slice(), utf16.as_slice(), TASK.as_bytes()] {
            assert_eq!(decode_output(bytes), TASK);
        }
    }
}
