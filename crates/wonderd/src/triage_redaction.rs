//! What `cupola triage` is allowed to write down.
//!
//! The triage file is meant to be handed to a person's coding agent and
//! quoted in a public issue, so everything that goes into it passes through
//! [`Redactor::scrub`] first. The rules, in the order they run:
//!
//! 1. A pairing link (any URL with a `#fragment`, a `/pair` path or a custom
//!    scheme such as `cupola://`) becomes `<pairing-link>`; other URLs lose
//!    their user, password, query and fragment.
//! 2. A value assigned to a secret-looking name (`token=…`, `"apiKey": "…"`,
//!    `Authorization: Bearer …`), a vendor token (`sk-…`, `ghp_…`, `tskey-…`,
//!    JWTs) and any opaque 32+ character run containing digits and letters
//!    become `<secret>`.
//! 3. Conversation text carried by JSON fields such as `"text"`, `"prompt"` or
//!    `"content"` becomes `<omitted>`, and no line is longer than
//!    [`MAX_LINE`] characters.
//! 4. Home directories (`$HOME`, `/home/<user>`, `/Users/<user>`,
//!    `C:\Users\<user>`) become `~`.
//! 5. Tailscale addresses become `<tailscale-ip>`, private network addresses
//!    `<lan-ip>`, other addresses `<ip>` (loopback stays).
//! 6. Tailscale/MagicDNS and local-network host names become `<hostname>`;
//!    the names this computer and its phones go by, supplied by the caller,
//!    become `<hostname>` and `<device-name>`; e-mail addresses `<email>`.
//!    Device and thread ids (UUIDs) fall to the opaque-run rule above.
//!
//! 7. While the host is stopped the phones' names are unknown, so log lines
//!    that mention a device, a connection or a network address (a name can
//!    stand alone: "Kitchen connected from 192.168.1.44") are dropped
//!    ([`mentions_device`]).
//!
//! Unit tests build their inputs from realistic host log lines and fail when
//! a rule is removed.
use regex::{Captures, Regex};
use serde_json::Value;
use std::{net::IpAddr, sync::OnceLock};

/// The longest line the triage file keeps, in characters.
pub const MAX_LINE: usize = 400;

/// What the caller knows about this computer that no pattern can recognise.
#[derive(Debug, Default, Clone)]
pub struct Redactor {
    home: Option<String>,
    hostnames: Vec<String>,
    device_names: Vec<String>,
}

fn rx(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("triage redaction pattern"))
}

/// A named redaction pass over one log line.
type Stage<'a> = (&'a str, &'a dyn Fn(&str) -> String);

impl Redactor {
    /// `home` is this user's home directory; `hostnames` the computer's own
    /// names; `device_names` paired phone names and sign-in accounts.
    pub fn new(
        home: Option<&str>,
        hostnames: impl IntoIterator<Item = String>,
        device_names: impl IntoIterator<Item = String>,
    ) -> Self {
        let keep = |name: &String| name.trim().chars().count() >= 3;
        let mut hostnames: Vec<String> = hostnames.into_iter().filter(keep).collect();
        let mut device_names: Vec<String> = device_names.into_iter().filter(keep).collect();
        // Longest first, so "Sam's iPhone 15" goes before "Sam's iPhone".
        hostnames.sort_by_key(|name| std::cmp::Reverse(name.len()));
        device_names.sort_by_key(|name| std::cmp::Reverse(name.len()));
        Self {
            home: home
                .map(|home| home.trim_end_matches(['/', '\\']).to_owned())
                .filter(|home| home.len() > 1),
            hostnames,
            device_names,
        }
    }

    /// `text` with everything the module documentation lists replaced.
    pub fn scrub(&self, text: &str) -> String {
        text.lines()
            .map(|line| self.scrub_line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every string in `value`, keys excepted, scrubbed in place.
    pub fn scrub_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.scrub(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.scrub_value(item)),
            Value::Object(map) => {
                for (key, item) in map.iter_mut() {
                    if is_conversation_field(key) && !disabled("conversation") {
                        if matches!(item, Value::String(_) | Value::Array(_) | Value::Object(_)) {
                            *item = Value::String("<omitted>".to_owned());
                        }
                    } else {
                        self.scrub_value(item);
                    }
                }
            }
            _ => {}
        }
    }

    fn scrub_line(&self, line: &str) -> String {
        let stages: [Stage<'_>; 10] = [
            ("links", &links),
            ("secrets", &secrets),
            ("conversation", &conversation),
            ("home-folder", &|t| self.home_folder(t)),
            ("user-folders", &user_folders),
            ("addresses", &addresses),
            ("mesh-hostnames", &mesh_hostnames),
            ("computer-names", &|t| {
                self.names(t, &self.hostnames, "<hostname>")
            }),
            ("phone-names", &|t| {
                self.names(t, &self.device_names, "<device-name>")
            }),
            ("emails", &emails),
        ];
        let mut text = line.to_owned();
        for (name, stage) in stages {
            if !disabled(name) {
                text = stage(&text);
            }
        }
        if disabled("line-length") {
            text
        } else {
            truncate(&text)
        }
    }

    fn home_folder(&self, text: &str) -> String {
        let Some(home) = &self.home else {
            return text.to_owned();
        };
        let text = replace_ci(text, home, "~");
        // The same folder as Windows writes it with the other slash.
        let flipped = if home.contains('\\') {
            home.replace('\\', "/")
        } else {
            home.replace('/', "\\")
        };
        replace_ci(&text, &flipped, "~")
    }

    fn names(&self, text: &str, names: &[String], with: &str) -> String {
        names
            .iter()
            .fold(text.to_owned(), |text, name| replace_ci(&text, name, with))
    }
}

/// `/home/<user>`, `/Users/<user>` and `C:\Users\<user>` for any user.
fn user_folders(text: &str) -> String {
    static UNIX: OnceLock<Regex> = OnceLock::new();
    static WINDOWS: OnceLock<Regex> = OnceLock::new();
    let text = rx(&UNIX, r"(?:/home/[^/\s:;,]+|/Users/[^/\s:;,]+|/root\b)").replace_all(text, "~");
    rx(&WINDOWS, r"(?i)[A-Z]:[\\/]+Users[\\/]+[^\\/\s:;,]+")
        .replace_all(&text, "~")
        .into_owned()
}

fn mesh_hostnames(text: &str) -> String {
    static MESH: OnceLock<Regex> = OnceLock::new();
    rx(
        &MESH,
        r"(?i)\b(?:[a-z0-9_-]+\.)+(?:ts\.net|local|lan|home\.arpa|internal|localdomain)\b",
    )
    .replace_all(text, "<hostname>")
    .into_owned()
}

fn emails(text: &str) -> String {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    rx(
        &EMAIL,
        r"[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+",
    )
    .replace_all(text, "<email>")
    .into_owned()
}

#[cfg(test)]
thread_local! {
    /// The one stage a test switches off to prove that stage is needed.
    static DISABLED: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

fn disabled(stage: &str) -> bool {
    #[cfg(test)]
    {
        DISABLED.with(|off| off.get() == Some(stage))
    }
    #[cfg(not(test))]
    {
        let _ = stage;
        false
    }
}

/// Case-insensitive literal replacement.
fn replace_ci(text: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    match Regex::new(&format!("(?i){}", regex::escape(needle))) {
        Ok(pattern) => pattern.replace_all(text, with).into_owned(),
        Err(_) => text.to_owned(),
    }
}

fn links(text: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    rx(&URL, r#"(?i)\b([a-z][a-z0-9+.-]*)://([^\s"'<>)\]]*)"#)
        .replace_all(text, |caps: &Captures| {
            let scheme = caps[1].to_ascii_lowercase();
            let rest = &caps[2];
            let web = scheme == "http" || scheme == "https" || scheme == "ws" || scheme == "wss";
            // A query or fragment can follow the host directly: `host?code=…`.
            let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let pairing = rest[path_start..].starts_with("/pair") || rest.contains('#');
            if !web || pairing {
                return "<pairing-link>".to_owned();
            }
            let authority = &rest[..path_start];
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let path = &rest[path_start..];
            let path = path.split(['?', '#']).next().unwrap_or("");
            let query = if rest.contains('?') {
                "?<redacted>"
            } else {
                ""
            };
            format!("{scheme}://{host}{path}{query}")
        })
        .into_owned()
}

fn secrets(text: &str) -> String {
    static ASSIGNED: OnceLock<Regex> = OnceLock::new();
    static HEADER: OnceLock<Regex> = OnceLock::new();
    static VENDOR: OnceLock<Regex> = OnceLock::new();
    static OPAQUE: OnceLock<Regex> = OnceLock::new();
    let text =
        rx(&HEADER, r"(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]{6,}").replace_all(text, "$1 <secret>");
    let text = rx(
        &ASSIGNED,
        r#"(?i)((?:"|')?[\w.-]*(?:token|secret|password|passwd|passphrase|api[_-]?key|apikey|access[_-]?key|private[_-]?key|credential|capability|cookie|signature|authorization|pairing[_-]?code|verification[_-]?code|host[_-]?key[_-]?pin|voice[_-]?key|nonce)[\w.-]*(?:"|')?\s*[:=]\s*(?:(?:bearer|basic)\s+)?)("(?:[^"\\]|\\.)*"|'[^']*'|[^\s,;}&]+)"#,
    )
    .replace_all(&text, |caps: &Captures| {
        // Keep the quotes, so JSON stays JSON.
        let quote = caps[2].chars().next().filter(|c| *c == '"' || *c == '\'');
        match quote {
            Some(q) => format!("{}{q}<secret>{q}", &caps[1]),
            None => format!("{}<secret>", &caps[1]),
        }
    });
    let text = rx(
        &VENDOR,
        r"\b(?:sk|pk|rk)-[A-Za-z0-9_-]{12,}|\bgh[pousr]_[A-Za-z0-9]{16,}|\bgithub_pat_[A-Za-z0-9_]{16,}|\btskey-[A-Za-z0-9-]{8,}|\bxox[baprs]-[A-Za-z0-9-]{8,}|\bAKIA[0-9A-Z]{16}\b|\beyJ[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]*",
    )
    .replace_all(&text, "<secret>");
    // Keys, pins and pairing secrets are long runs of letters and digits.
    rx(&OPAQUE, r"[A-Za-z0-9_-]{32,}")
        .replace_all(&text, |caps: &Captures| {
            let run = &caps[0];
            let letters = run.chars().any(|c| c.is_ascii_alphabetic());
            let digits = run.chars().any(|c| c.is_ascii_digit());
            if letters && digits {
                "<secret>".to_owned()
            } else {
                run.to_owned()
            }
        })
        .into_owned()
}

/// JSON fields that carry what an agent said or was told.
const CONVERSATION_FIELDS: &[&str] = &[
    "text",
    "content",
    "prompt",
    "input",
    "output",
    "body",
    "delta",
    "transcript",
    "patch",
    "diff",
    "arguments",
    "instructions",
    "reply",
];

/// Whether a JSON key names a conversation field.
fn is_conversation_field(key: &str) -> bool {
    CONVERSATION_FIELDS
        .iter()
        .any(|field| key.eq_ignore_ascii_case(field))
}

/// What an agent said or was told, as structured logs carry it. A field's
/// string, object or array value is replaced whole, nested content included;
/// a value cut off by the end of the line goes to the end of the line.
fn conversation(text: &str) -> String {
    static FIELD: OnceLock<Regex> = OnceLock::new();
    let field = rx(&FIELD, r#"(?i)"([a-z_]+)"\s*:\s*"#);
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(found) = field.captures_at(text, from) {
        let whole = found.get(0).expect("whole match");
        from = whole.end();
        if whole.start() < last || !is_conversation_field(&found[1]) {
            continue;
        }
        let Some(end) = json_value_end(text, whole.end()) else {
            continue;
        };
        out.push_str(&text[last..whole.end()]);
        out.push_str("\"<omitted>\"");
        last = end;
        from = end;
    }
    out.push_str(&text[last..]);
    out
}

/// Where the JSON string, object or array starting at `start` ends, or the
/// end of `text` when it is cut off. `None` for numbers, booleans and null.
fn json_value_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let open = *bytes.get(start)?;
    if !matches!(open, b'"' | b'{' | b'[') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => {
                    in_string = false;
                    if depth == 0 {
                        return Some(start + offset + 1);
                    }
                }
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    Some(text.len())
}

/// Whether a log line speaks of a phone or paired device, or of a connection
/// or a remote address, which is where a phone's name shows up even with no
/// device word ("Kitchen connected from 192.168.1.44"). Phone names are only
/// known while the host answers; without them such a line may carry one that
/// no pattern can find ("Device Maria's iPhone 15 connected"), so the caller
/// drops it rather than guessing where the name ends.
pub fn mentions_device(line: &str) -> bool {
    static DEVICE: OnceLock<Regex> = OnceLock::new();
    static POSSESSIVE: OnceLock<Regex> = OnceLock::new();
    const NOT_NAMES: &[&str] = &[
        "it", "that", "there", "what", "let", "here", "who", "he", "she", "how", "where", "when",
    ];
    rx(
        &DEVICE,
        r"(?i)\b(?:devices?|phones?|iphones?|ipads?|android|pixel|galaxy|tablets?|paired|pairing|labels?|named|device[_-]?name|(?:dis)?connect(?:ed|ing|s|ions?)?|accept(?:ed|s)?|reject(?:ed|s)?|handshakes?|peers?|clients?|joined|authenticated|authorized|attached|subscribed)\b",
    )
    .is_match(line)
        || addresses(line) != line
        || rx(&POSSESSIVE, r"(?i)\b([\p{L}\p{N}_-]+)['’]s\b")
            .captures_iter(line)
            .any(|caps| !NOT_NAMES.contains(&caps[1].to_lowercase().as_str()))
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_LINE {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_LINE).collect();
    format!("{kept}…[truncated]")
}

fn address_label(address: IpAddr) -> Option<&'static str> {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_loopback() || v4.is_unspecified() {
                None
            } else if a == 100 && (64..128).contains(&b) {
                Some("<tailscale-ip>")
            } else if v4.is_private() || v4.is_link_local() {
                Some("<lan-ip>")
            } else {
                Some("<ip>")
            }
        }
        IpAddr::V6(v6) => {
            let first = v6.segments();
            if v6.is_loopback() || v6.is_unspecified() {
                None
            } else if first[0] == 0xfd7a && first[1] == 0x115c && first[2] == 0xa1e0 {
                Some("<tailscale-ip>")
            } else if first[0] & 0xfe00 == 0xfc00 || first[0] & 0xffc0 == 0xfe80 {
                Some("<lan-ip>")
            } else {
                Some("<ip>")
            }
        }
    }
}

fn addresses(text: &str) -> String {
    static V4: OnceLock<Regex> = OnceLock::new();
    static V6: OnceLock<Regex> = OnceLock::new();
    let text = rx(&V4, r"\b(?:\d{1,3}\.){3}\d{1,3}\b").replace_all(text, |caps: &Captures| {
        match caps[0].parse::<IpAddr>().ok().and_then(address_label) {
            Some(label) => label.to_owned(),
            None => caps[0].to_owned(),
        }
    });
    // Candidates are runs of hex digits, colons and dots; only a whole run
    // that parses as an address counts, so "12:30:45" and "std::io" stay.
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in rx(&V6, r"[0-9A-Fa-f:.]*:[0-9A-Fa-f:.]*").find_iter(&text) {
        let start = found.start();
        // The whole run first ("fe80::" is an address), then without the
        // punctuation of a sentence that ends in one.
        let whole = found.as_str();
        let trimmed = whole.trim_end_matches(['.', ':']);
        let Some((end, label)) = [whole, trimmed].into_iter().find_map(|candidate| {
            let label = candidate.parse::<IpAddr>().ok().and_then(address_label)?;
            Some((start + candidate.len(), label))
        }) else {
            continue;
        };
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if before.is_some_and(word) || after.is_some_and(word) {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(label);
        last = end;
        // A zone such as `%en0` belongs to the address.
        if text[last..].starts_with('%') {
            let zone = text[last + 1..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
                .unwrap_or(text.len() - last - 1);
            last += 1 + zone;
        }
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod triage_redaction_tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::new(
            // Not under /home or /Users, so only the known folder can catch it.
            Some("/data/maria"),
            ["maria-mbp".to_owned(), "Marias-MacBook-Pro".to_owned()],
            ["Maria's iPhone 15".to_owned()],
        )
    }

    /// Realistic host log lines, each carrying one kind of private value.
    /// The second column is what must not survive.
    const LEAKS: &[(&str, &str)] = &[
        (
            "2026-11-01 09:14:02.118 -0700 Authorization: Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0 rejected for /api/v1/rpc",
            "9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        ),
        (
            "2026-11-01 09:14:02.118 -0700 loopback capability=Zx83kQ_pL0aW1sD9fGh2JkLmNb4VcXz7 mismatch",
            "Zx83kQ_pL0aW1sD9fGh2JkLmNb4VcXz7",
        ),
        (
            r#"2026-11-01 09:14:02.118 -0700 config {"apiKey":"abc123def456","tunnel":"tailscale"}"#,
            "abc123def456",
        ),
        (
            "2026-11-01 09:15:40.002 -0700 Offered https://maria-mbp.tail1a2b3.ts.net/pair#Qm9ndXNQYWlyaW5nU2VjcmV0MTIzNDU2Nzg5MA",
            "Qm9ndXNQYWlyaW5nU2VjcmV0",
        ),
        (
            "2026-11-01 09:15:40.002 -0700 Offered https://192.168.1.20:3778/pair#a1b2c3",
            "a1b2c3",
        ),
        (
            "2026-11-01 09:15:41.500 -0700 Tailscale route ready at https://maria-mbp.tail1a2b3.ts.net",
            "tail1a2b3",
        ),
        (
            "2026-11-01 09:16:00.001 -0700 Serving at http://100.101.102.103:3777",
            "100.101.102.103",
        ),
        (
            "2026-11-01 09:16:00.002 -0700 Home network listening on 192.168.1.20:3778 and 10.0.0.7:3778",
            "192.168.1.20",
        ),
        (
            "2026-11-01 09:16:00.003 -0700 Tailscale peer fd7a:115c:a1e0::1234:5678 unreachable",
            "fd7a:115c:a1e0",
        ),
        (
            "2026-11-01 09:16:00.004 -0700 Public address 203.0.113.9 seen by STUN",
            "203.0.113.9",
        ),
        (
            "2026-11-01 09:17:12.310 -0700 Cupola host 0.1.0 starting (pid 4121, data /home/maria/.cupola)",
            "/home/maria",
        ),
        (
            "2026-11-01 09:17:12.310 -0700 Cupola host 0.1.0 starting (pid 4121, data /data/maria/.cupola)",
            "/data/maria",
        ),
        (
            "2026-11-01 09:18:31.500 -0700 name conflict on Marias-MacBook-Pro, retrying",
            "Marias-MacBook-Pro",
        ),
        (
            "2026-11-01 09:17:12.311 -0700 codex at /Users/maria/Library/Application Support/Codex/bin/codex failed",
            "/Users/maria",
        ),
        (
            r"2026-11-01 09:17:12.312 -0700 helper C:\Users\Maria\AppData\Local\Cupola\computer-helper.exe exited",
            r"Users\Maria",
        ),
        (
            "2026-11-01 09:18:30.700 -0700 Device Maria's iPhone 15 connected over relay",
            "Maria's iPhone 15",
        ),
        (
            "2026-11-01 09:18:31.000 -0700 mDNS announce Marias-MacBook-Pro.local",
            "Marias-MacBook-Pro",
        ),
        (
            "2026-11-01 09:18:32.000 -0700 Signed in as maria@example.com",
            "maria@example.com",
        ),
        (
            "2026-11-01 09:18:33.000 -0700 revoked device 0b6f2f1e-5f7c-4f0a-9d52-3c1d2f7a9e10",
            "0b6f2f1e-5f7c-4f0a-9d52-3c1d2f7a9e10",
        ),
        (
            "2026-11-01 09:19:00.000 -0700 turn failed {\"type\":\"agentMessage\",\"text\":\"Here is the password for the staging db hunter2\"}",
            "hunter2",
        ),
        (
            "2026-11-01 09:19:01.000 -0700 request https://api.example.com/v1/models?api_key=SECRETQUERY&x=1 failed",
            "SECRETQUERY",
        ),
        (
            "2026-11-01 09:19:01.500 -0700 callback https://relay.example.com?code=PRIVATECODE&state=x failed",
            "PRIVATECODE",
        ),
        (
            "2026-11-01 09:19:01.600 -0700 callback https://relay.example.com#frag=PRIVATEFRAG failed",
            "PRIVATEFRAG",
        ),
        (
            r#"2026-11-01 09:19:04.000 -0700 tool call {"name":"shell","arguments":{"cmd":"cat ~/notes","env":{"NOTE":"NESTEDARG"}}}"#,
            "NESTEDARG",
        ),
        (
            r#"2026-11-01 09:19:05.000 -0700 turn {"content":[{"type":"text","value":"ARRAYBODY"},"second \"quoted\" ARRAYTAIL"],"state":"done"}"#,
            "ARRAYBODY",
        ),
        (
            r#"2026-11-01 09:19:05.500 -0700 turn {"input":{"a":["CUTOFFVALUE"#,
            "CUTOFFVALUE",
        ),
        (
            "2026-11-01 09:19:02.000 -0700 sign-in used key sk-proj-AbCdEfGhIjKlMnOpQrStUv and ghp_AbCdEfGhIjKlMnOpQrStUvWx",
            "AbCdEfGhIjKlMnOp",
        ),
        (
            "2026-11-01 09:19:03.000 -0700 host key pin 3q2-7wEfT9zYpV1sR4uHn8LdKcM0bXaJgQiNoPtUyWk",
            "3q2-7wEfT9zYpV1sR4uHn8LdKcM0bXaJgQiNoPtUyWk",
        ),
    ];

    #[test]
    fn realistic_log_lines_lose_every_private_value() {
        let redactor = redactor();
        for (line, leak) in LEAKS {
            let scrubbed = redactor.scrub(line);
            assert!(
                !scrubbed.contains(leak),
                "`{leak}` survived in: {scrubbed}\n(from: {line})"
            );
        }
    }

    #[test]
    fn scrubbed_lines_keep_what_a_diagnosis_needs() {
        let redactor = redactor();
        let scrub = |line: &str| redactor.scrub(line);
        assert_eq!(
            scrub("2026-11-01 09:17:12.310 -0700 Cupola host 0.1.0 starting (pid 4121, data /home/maria/.cupola)"),
            "2026-11-01 09:17:12.310 -0700 Cupola host 0.1.0 starting (pid 4121, data ~/.cupola)"
        );
        assert_eq!(
            scrub("Serving at http://100.101.102.103:3777"),
            "Serving at http://<tailscale-ip>:3777"
        );
        assert_eq!(
            scrub("listening on 192.168.1.20:3778, 172.20.0.5 and 127.0.0.1:3777"),
            "listening on <lan-ip>:3778, <lan-ip> and 127.0.0.1:3777"
        );
        assert_eq!(scrub("peer 203.0.113.9"), "peer <ip>");
        assert_eq!(
            scrub("Offered https://maria-mbp.tail1a2b3.ts.net/pair#Qm9ndXNQYWly"),
            "Offered <pairing-link>"
        );
        assert_eq!(
            scrub("Device Maria's iPhone 15 connected"),
            "Device <device-name> connected"
        );
        assert_eq!(
            scrub("Tailscale route ready at https://maria-mbp.tail1a2b3.ts.net"),
            "Tailscale route ready at https://<hostname>"
        );
        assert_eq!(
            scrub("The host stopped unexpectedly (exit status: 101); restarting it in 1.0 s"),
            "The host stopped unexpectedly (exit status: 101); restarting it in 1.0 s"
        );
        assert_eq!(
            scrub("12:30:45 std::io::Error at [::1]:3777 and fe80::1%en0 down"),
            "12:30:45 std::io::Error at [::1]:3777 and <lan-ip> down"
        );
        assert_eq!(
            scrub("Authorization: Bearer abcdef0123456789"),
            "Authorization: Bearer <secret>"
        );
        assert_eq!(
            scrub("sent Bearer abcdef0123456789 upstream"),
            "sent Bearer <secret> upstream"
        );
        assert_eq!(
            scrub(r#"{"token":"abcd1234","state":"ready"}"#),
            r#"{"token":"<secret>","state":"ready"}"#
        );
    }

    #[test]
    fn structured_conversation_fields_go_whole_and_json_stays_valid() {
        let scrub = |line: &str| redactor().scrub(line);
        assert_eq!(
            scrub(r#"{"arguments":{"cmd":"ls","env":{"A":"b"}},"state":"done"}"#),
            r#"{"arguments":"<omitted>","state":"done"}"#
        );
        assert_eq!(
            scrub(r#"{"content":[{"text":"hi"},"a \"b\" c"],"n":1}"#),
            r#"{"content":"<omitted>","n":1}"#
        );
        // Numbers and booleans carry no conversation.
        assert_eq!(
            scrub(r#"{"output":0,"input":null}"#),
            r#"{"output":0,"input":null}"#
        );
    }

    #[test]
    fn urls_without_a_path_lose_their_query_and_fragment() {
        let scrub = |line: &str| redactor().scrub(line);
        assert_eq!(
            scrub("callback https://relay.example.com?code=PRIVATECODE&state=x failed"),
            "callback https://relay.example.com?<redacted> failed"
        );
        assert_eq!(
            scrub("callback https://relay.example.com#frag=PRIVATEFRAG failed"),
            "callback <pairing-link> failed"
        );
    }

    #[test]
    fn device_lines_are_recognised_without_a_name_list() {
        for line in [
            "Device Maria's iPhone 15 connected over relay",
            "paired Sam’s phone",
            "label=Kitchen iPad accepted",
            "Kitchen connected from 192.168.1.44",
            "Kitchen reached the host at fe80::1c2d:3e4f:5a6b:7c8d%en0",
            "Bedroom disconnected",
        ] {
            assert!(mentions_device(line), "{line}");
        }
        for line in [
            "The host stopped unexpectedly (signal: 11); restarting it in 1.0 s",
            "it's ready; that's fine",
        ] {
            assert!(!mentions_device(line), "{line}");
        }
    }

    #[test]
    fn whole_values_are_scrubbed_but_keys_stay() {
        let mut value = serde_json::json!({
            "lines": ["listening on 192.168.1.20:3778"],
            "nested": {"detail": "opened /Users/maria/Code/app", "count": 3}
        });
        redactor().scrub_value(&mut value);
        assert_eq!(value["lines"][0], "listening on <lan-ip>:3778");
        assert_eq!(value["nested"]["detail"], "opened ~/Code/app");
        assert_eq!(value["nested"]["count"], 3);
        let mut value = serde_json::json!({"arguments": {"cmd": "ls"}, "content": ["a"], "n": 2});
        redactor().scrub_value(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"arguments": "<omitted>", "content": "<omitted>", "n": 2})
        );
    }

    /// Each stage earns its place: with it switched off, a line in the table
    /// above (or a long line) gets through.
    #[test]
    fn every_redaction_stage_is_needed() {
        let long = "x ".repeat(500);
        let stages = [
            "links",
            "secrets",
            "conversation",
            "home-folder",
            "user-folders",
            "addresses",
            "mesh-hostnames",
            "computer-names",
            "phone-names",
            "emails",
            "line-length",
        ];
        for stage in stages {
            DISABLED.with(|off| off.set(Some(stage)));
            let redactor = redactor();
            let leaked = LEAKS
                .iter()
                .any(|(line, leak)| redactor.scrub(line).contains(leak))
                || redactor.scrub(&long).chars().count() > MAX_LINE + 20;
            DISABLED.with(|off| off.set(None));
            assert!(
                leaked,
                "switching off `{stage}` leaks nothing: the stage is dead weight"
            );
        }
    }

    #[test]
    fn long_lines_are_cut() {
        let line = "x ".repeat(500);
        let scrubbed = redactor().scrub(&line);
        assert!(
            scrubbed.chars().count() < MAX_LINE + 20,
            "{}",
            scrubbed.len()
        );
        assert!(scrubbed.ends_with("[truncated]"));
    }

    #[test]
    fn short_names_are_not_treated_as_secrets_or_devices() {
        // Two-letter names would erase ordinary words.
        let redactor = Redactor::new(None, ["ab".to_owned()], ["me".to_owned()]);
        assert_eq!(redactor.scrub("able to name me"), "able to name me");
    }
}
