//! The recovery endpoint's request and response types.
//!
//! The host supervisor serves these while the host daemon is stopped, gave up
//! restarting or hangs, so a paired phone can read its state, ask for a
//! restart and fetch redacted diagnostics. The command set is fixed: there is
//! no way to name a command, a path or a file.
//!
//! Every request is a POST to `/api/v1/rescue/{status,restart,diagnostics}`
//! whose JSON body is a [`RescueRequest`]. The device signs the same
//! `wonder-action-v1` transcript it signs for other consequential actions
//! ([`crate::pairing::ActionTranscript`]); [`RescueCommand::transcript_bytes`]
//! builds it so the host and tests cannot disagree with the phone.

use serde::{Deserialize, Serialize};

use crate::pairing::{canonical_action_transcript, ActionTranscript};

/// The `sessionBinding` field of a rescue transcript. A rescue request has no
/// session (the daemon that issues sessions may be down), so this fixed value
/// keeps a rescue signature from standing in for any session-bound action.
pub const RESCUE_SESSION_BINDING: &str = "rescue-v1";

/// SHA-256 of the empty string: rescue requests carry no semantic body.
pub const RESCUE_BODY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A signed request is accepted within this long of its `issuedAtMs`.
pub const RESCUE_WINDOW_MS: u64 = 60_000;

/// The whole command set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RescueCommand {
    Status,
    Restart,
    Diagnostics,
}

impl RescueCommand {
    pub const ALL: [Self; 3] = [Self::Status, Self::Restart, Self::Diagnostics];

    /// The last path segment, and what the phone names the command.
    pub fn name(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Restart => "restart",
            Self::Diagnostics => "diagnostics",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.name() == name)
    }

    /// The signed `action`.
    pub fn action(self) -> &'static str {
        match self {
            Self::Status => "rescue.status",
            Self::Restart => "rescue.restart",
            Self::Diagnostics => "rescue.diagnostics",
        }
    }

    /// The signed `target`, which is also the request path.
    pub fn path(self) -> String {
        format!("/api/v1/rescue/{}", self.name())
    }

    /// What a device signs for this command. `target` is [`Self::path`].
    pub fn transcript<'a>(
        self,
        target: &'a str,
        device_id: &'a str,
        host_installation_id: &'a str,
        action_nonce: &'a str,
        issued_at_ms: u64,
    ) -> ActionTranscript<'a> {
        ActionTranscript {
            action: self.action(),
            target,
            body_sha256: RESCUE_BODY_SHA256,
            action_nonce,
            session_binding: RESCUE_SESSION_BINDING,
            device_id,
            host_installation_id,
            issued_at_ms,
            expected_state: "",
        }
    }

    /// The bytes a device signs for this command.
    pub fn transcript_bytes(
        self,
        device_id: &str,
        host_installation_id: &str,
        action_nonce: &str,
        issued_at_ms: u64,
    ) -> Vec<u8> {
        let target = self.path();
        canonical_action_transcript(&self.transcript(
            &target,
            device_id,
            host_installation_id,
            action_nonce,
            issued_at_ms,
        ))
    }
}

/// The JSON body of every rescue request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueRequest {
    pub device_id: String,
    /// 16 to 128 characters of `A-Za-z0-9_-`; never reused.
    pub action_nonce: String,
    pub issued_at_ms: u64,
    /// Unpadded base64url of the raw 64-byte P-256 `r || s` signature.
    pub signature: String,
}

/// Where the host stands, as the supervisor sees it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RescueState {
    /// The daemon is running and answering.
    Running,
    /// The daemon is starting, or waits to be replaced after a crash.
    Starting,
    /// The supervisor is stopping the host.
    Stopped,
    /// The daemon crashed too often to be restarted again. A restart request
    /// clears this.
    GaveUp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueStatus {
    pub state: RescueState,
    /// Why the daemon last stopped, in plain words; null before it did.
    pub last_exit: Option<String>,
    /// Crashes within the supervisor's current restart window.
    pub crash_count: u32,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueRestartResponse {
    /// The supervisor will start the daemon now.
    pub accepted: bool,
    pub state: RescueState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueDiagnostics {
    pub status: RescueStatus,
    pub generated_at_ms: u64,
    /// The newest redacted host log lines, oldest first.
    pub log_lines: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RescueErrorCode {
    /// Not a paired device, a revoked or expired one, a bad signature, or a
    /// request already used. One answer for all of them.
    Unauthorized,
    /// `issuedAtMs` is outside the window: the phone's clock is off or the
    /// request is old.
    Stale,
    /// Malformed JSON, an unknown field, or an unusable nonce.
    BadRequest,
    /// Too many restarts; see `retryAfterSeconds`.
    RateLimited,
    /// This platform's supervisor cannot do that.
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueError {
    pub error: RescueErrorCode,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_body_hash_is_sha256_of_nothing() {
        use sha2::{Digest, Sha256};
        assert_eq!(hex::encode(Sha256::digest(b"")), RESCUE_BODY_SHA256);
    }

    #[test]
    fn each_command_signs_its_own_action_and_path() {
        let bytes: Vec<_> = RescueCommand::ALL
            .map(|command| command.transcript_bytes("phone", "host", "nonce", 7))
            .into();
        assert_eq!(
            String::from_utf8(bytes[1].clone()).unwrap(),
            format!(
                "wonder-action-v1\nrescue.restart\n/api/v1/rescue/restart\n{RESCUE_BODY_SHA256}\nnonce\nrescue-v1\nphone\nhost\n7\n"
            )
        );
        assert_ne!(bytes[0], bytes[1]);
        assert_ne!(bytes[1], bytes[2]);
        assert_eq!(
            RescueCommand::from_name("restart"),
            Some(RescueCommand::Restart)
        );
        assert_eq!(RescueCommand::from_name("shell"), None);
    }
}
