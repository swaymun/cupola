//! Canonical bytes for Wonder's signed pairing/session challenge transcript.
//!
//! This module deliberately stops at transcript construction. Hashing, signing,
//! challenge lifetime, and replay protection belong to the surrounding pairing
//! protocol.

/// The protocol domain included at the start of every Wonder session transcript.
pub const PROTOCOL_DOMAIN: &str = "wonder-session-v1";
pub const ACTION_PROTOCOL_DOMAIN: &str = "wonder-action-v1";

/// Values covered by a signed Wonder session or consequential-action request.
///
/// The fields are serialized in declaration order as UTF-8 strings separated by
/// a single LF byte. The encoding is intentionally not JSON so that equivalent
/// values cannot acquire different signed representations through serializer
/// behavior or object-key ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChallengeTranscript<'a> {
    pub device_id: &'a str,
    pub challenge_id: &'a str,
    pub nonce: &'a str,
    pub origin: &'a str,
    pub host_installation_id: &'a str,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

impl<'a> ChallengeTranscript<'a> {
    /// Constructs the exact bytes covered by the device signature.
    ///
    /// The result is UTF-8 and has no trailing newline. Callers should provide
    /// already-canonical protocol values; this function performs no cryptography
    /// or normalization.
    pub fn to_bytes(&self) -> Vec<u8> {
        let issued_at_ms = self.issued_at_ms.to_string();
        let expires_at_ms = self.expires_at_ms.to_string();
        let fields = [
            PROTOCOL_DOMAIN,
            self.device_id,
            self.challenge_id,
            self.nonce,
            self.origin,
            self.host_installation_id,
            &issued_at_ms,
            &expires_at_ms,
        ];

        fields.join("\n").into_bytes()
    }
}

/// Builds canonical `wonder-session-v1` transcript bytes.
pub fn canonical_challenge_transcript(transcript: &ChallengeTranscript<'_>) -> Vec<u8> {
    transcript.to_bytes()
}

/// Values covered by a detached signature for a consequential Wonder action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionTranscript<'a> {
    pub action: &'a str,
    pub target: &'a str,
    pub body_sha256: &'a str,
    pub action_nonce: &'a str,
    pub session_binding: &'a str,
    pub device_id: &'a str,
    pub host_installation_id: &'a str,
    pub issued_at_ms: u64,
    pub expected_state: &'a str,
}

impl ActionTranscript<'_> {
    pub fn to_bytes(&self) -> Vec<u8> {
        [
            ACTION_PROTOCOL_DOMAIN,
            self.action,
            self.target,
            self.body_sha256,
            self.action_nonce,
            self.session_binding,
            self.device_id,
            self.host_installation_id,
            &self.issued_at_ms.to_string(),
            self.expected_state,
        ]
        .join("\n")
        .into_bytes()
    }
}

pub fn canonical_action_transcript(transcript: &ActionTranscript<'_>) -> Vec<u8> {
    transcript.to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> ChallengeTranscript<'static> {
        ChallengeTranscript {
            device_id: "device-01",
            challenge_id: "challenge-01",
            nonce: "nonce-01",
            origin: "https://wonder.example.ts.net",
            host_installation_id: "install-1",
            issued_at_ms: 1_000,
            expires_at_ms: 61_000,
        }
    }

    #[test]
    fn transcript_bytes_are_deterministic_and_lf_delimited() {
        let transcript = fixture();
        let expected = concat!(
            "wonder-session-v1\n",
            "device-01\n",
            "challenge-01\n",
            "nonce-01\n",
            "https://wonder.example.ts.net\n",
            "install-1\n",
            "1000\n",
            "61000"
        )
        .as_bytes();

        assert_eq!(canonical_challenge_transcript(&transcript), expected);
        assert_eq!(transcript.to_bytes(), expected);
        assert!(!expected.ends_with(b"\n"));
    }

    #[test]
    fn action_transcript_is_domain_separated_and_ordered() {
        let transcript = ActionTranscript {
            action: "approval.resolve",
            target: "/api/v1/approvals/approval-1/resolve",
            body_sha256: "body-hash",
            action_nonce: "nonce-1",
            session_binding: "session-1",
            device_id: "device-01",
            host_installation_id: "install-1",
            issued_at_ms: 1_000,
            expected_state: "pending",
        };
        assert_eq!(
            canonical_action_transcript(&transcript),
            b"wonder-action-v1\napproval.resolve\n/api/v1/approvals/approval-1/resolve\nbody-hash\nnonce-1\nsession-1\ndevice-01\ninstall-1\n1000\npending"
        );
    }
}
