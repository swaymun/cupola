//! Pairing state and signature verification for the owner-only device protocol.

use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use p256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey, VerifyingKey,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::pairing::{
    canonical_action_transcript, canonical_challenge_transcript, ActionTranscript,
    ChallengeTranscript,
};

pub const OFFER_LIFETIME_MS: u64 = 5 * 60 * 1000;
pub const CHALLENGE_LIFETIME_MS: u64 = 60 * 1000;
/// How long an expired offer, challenge, enrollment or session is kept so a
/// late retry still gets its specific "expired" or "replayed" answer.
pub const PRUNE_AFTER_EXPIRY_MS: u64 = 10 * 60 * 1000;
/// Unanswered fresh challenges one device may hold at once; older ones go.
pub const MAX_OPEN_CHALLENGES_PER_DEVICE: usize = 16;
pub const NEVER_EXPIRES_AT_MS: u64 = i64::MAX as u64;
const HUMAN_CODE_ATTEMPT_WINDOW_MS: u64 = 60 * 1000;
const HUMAN_CODE_ATTEMPT_LIMIT: usize = 20;
pub const HOST_PROOF_DOMAIN: &str = "wonder-host-proof-v1";
const HOST_PROOF_NONCE_MIN_LEN: usize = 16;
const HOST_PROOF_NONCE_MAX_LEN: usize = 128;
const HOST_PROOF_ORIGIN_MAX_LEN: usize = 2048;
/// DER header of a P-256 SubjectPublicKeyInfo; an uncompressed 65-byte point follows.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DevicePublicKeyJwk {
    pub kty: String,
    pub crv: String,
    pub x: String,
    pub y: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingOffer {
    pub offer_id: String,
    pub url: String,
    pub expires_at_ms: u64,
    pub human_code: String,
    /// The link's `hostKey=`, also given here so callers need not parse it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_key_pin: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub challenge_id: String,
    pub device_id: String,
    pub nonce: String,
    pub origin: String,
    pub host_installation_id: String,
    pub offer_id: String,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedDevice {
    pub device_id: String,
    pub challenge: Challenge,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifiedSession {
    pub device_id: String,
    pub session_token: String,
    pub csrf_token: String,
    pub host_installation_id: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub enum PairingError {
    NotFound,
    Expired,
    AlreadyConsumed,
    InvalidSecret,
    InvalidPublicKey,
    InvalidSignature,
    InvalidSessionExpiration,
    ChallengeNotFound,
    ChallengeExpired,
    ChallengeReplayed,
    BindingMismatch,
    ConfirmationRequired,
    SessionNotFound,
    SessionExpired,
    CsrfMismatch,
}

struct OfferRecord {
    secret_hash: [u8; 32],
    human_code: String,
    origin: String,
    host_installation_id: String,
    expires_at_ms: u64,
    consumed: bool,
}

struct ChallengeRecord {
    challenge: Challenge,
    consumed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEnrollment {
    pub device_id: String,
    pub label: String,
    pub public_key: DevicePublicKeyJwk,
    pub challenge: Challenge,
    pub session_expires_at_ms: Option<u64>,
    /// The phone's hardware model ("iPhone17,2"), when it sends one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

struct DeviceRecord {
    confirmed: bool,
    public_key: DevicePublicKeyJwk,
    revoked: bool,
    session_expires_at_ms: Option<u64>,
}

struct SessionRecord {
    device_id: String,
    csrf_hash: [u8; 32],
    expires_at_ms: u64,
}

#[derive(Default)]
pub struct PairingState {
    offers: HashMap<String, OfferRecord>,
    challenges: HashMap<String, ChallengeRecord>,
    devices: HashMap<String, DeviceRecord>,
    sessions: HashMap<String, SessionRecord>,
    human_code_attempts: Vec<u64>,
    pending: HashMap<String, PendingEnrollment>,
}

impl PairingState {
    /// Drops offers, challenges, unconfirmed enrollments and sessions that
    /// expired more than `PRUNE_AFTER_EXPIRY_MS` ago. Called wherever a record
    /// is added, so each phone reconnect or pairing attempt cannot grow the
    /// state for the daemon's lifetime.
    fn prune(&mut self, now_ms: u64) {
        let live =
            |expires_at_ms: u64| now_ms < expires_at_ms.saturating_add(PRUNE_AFTER_EXPIRY_MS);
        self.offers.retain(|_, offer| live(offer.expires_at_ms));
        self.challenges
            .retain(|_, record| live(record.challenge.expires_at_ms));
        self.pending
            .retain(|_, pending| live(pending.challenge.expires_at_ms));
        // An unconfirmed device without a pending enrollment was rejected,
        // cancelled or expired and can never be confirmed.
        let pending = &self.pending;
        self.devices
            .retain(|device_id, device| device.confirmed || pending.contains_key(device_id));
        self.sessions
            .retain(|_, session| live(session.expires_at_ms));
    }

    pub fn restore_device(
        &mut self,
        device_id: String,
        public_key: DevicePublicKeyJwk,
        revoked: bool,
        session_expires_at_ms: Option<u64>,
    ) {
        self.devices.insert(
            device_id,
            DeviceRecord {
                confirmed: true,
                public_key,
                revoked,
                session_expires_at_ms,
            },
        );
    }

    pub fn restore_session(
        &mut self,
        token_hash: String,
        device_id: String,
        csrf_hash: [u8; 32],
        expires_at_ms: u64,
    ) {
        self.sessions.insert(
            token_hash,
            SessionRecord {
                device_id,
                csrf_hash,
                expires_at_ms,
            },
        );
    }

    pub fn create_offer(
        &mut self,
        origin: &str,
        host_installation_id: &str,
        now_ms: u64,
    ) -> PairingOffer {
        self.create_offer_with_host_key(origin, host_installation_id, None, now_ms)
    }

    /// Creates an offer whose link also carries the host key pin (`hostKey=`).
    /// Phones that parse exactly three fragment fields reject a fourth.
    pub fn create_offer_with_host_key(
        &mut self,
        origin: &str,
        host_installation_id: &str,
        host_key_pin: Option<&str>,
        now_ms: u64,
    ) -> PairingOffer {
        let offer_id = uuid::Uuid::new_v4().to_string();
        let secret = secret_bytes();
        let secret_encoded = URL_SAFE_NO_PAD.encode(secret);
        let expires_at_ms = now_ms.saturating_add(OFFER_LIFETIME_MS);
        self.prune(now_ms);
        self.offers.insert(
            offer_id.clone(),
            OfferRecord {
                secret_hash: digest(&secret),
                human_code: secret_encoded
                    .chars()
                    .take(10)
                    .collect::<String>()
                    .to_ascii_uppercase(),
                origin: origin.into(),
                host_installation_id: host_installation_id.into(),
                expires_at_ms,
                consumed: false,
            },
        );
        let human_code = secret_encoded.chars().take(10).collect();
        let host_key_field = host_key_pin
            .map(|pin| format!("&hostKey={pin}"))
            .unwrap_or_default();
        PairingOffer {
            url: format!("{origin}/pair#secret={secret_encoded}&offerId={offer_id}&hostInstallationId={host_installation_id}{host_key_field}"),
            offer_id,
            expires_at_ms,
            human_code,
            host_key_pin: host_key_pin.map(str::to_owned),
        }
    }

    pub fn claim_offer_with_session_expiration(
        &mut self,
        offer_id: &str,
        secret_encoded: &str,
        public_key: DevicePublicKeyJwk,
        label: &str,
        session_expires_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<ClaimedDevice, PairingError> {
        let secret = URL_SAFE_NO_PAD
            .decode(secret_encoded)
            .map_err(|_| PairingError::InvalidSecret)?;
        if secret.len() != 32 {
            return Err(PairingError::InvalidSecret);
        }
        let offer = self.offers.get(offer_id).ok_or(PairingError::NotFound)?;
        if digest(&secret).ct_eq(&offer.secret_hash).unwrap_u8() != 1 {
            return Err(PairingError::InvalidSecret);
        }
        self.claim_offer_record(offer_id, public_key, label, session_expires_at_ms, now_ms)
    }

    pub fn claim_offer_by_human_code(
        &mut self,
        human_code: &str,
        public_key: DevicePublicKeyJwk,
        label: &str,
        session_expires_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<ClaimedDevice, PairingError> {
        let normalized = human_code.trim().to_ascii_uppercase();
        let offer_id = self
            .offers
            .iter()
            .find(|(_, offer)| offer.human_code == normalized)
            .map(|(offer_id, _)| offer_id.clone())
            .ok_or(PairingError::NotFound)?;
        self.claim_offer_record(&offer_id, public_key, label, session_expires_at_ms, now_ms)
    }

    pub fn allow_human_code_attempt(&mut self, now_ms: u64) -> bool {
        self.human_code_attempts
            .retain(|attempt| now_ms.saturating_sub(*attempt) < HUMAN_CODE_ATTEMPT_WINDOW_MS);
        if self.human_code_attempts.len() >= HUMAN_CODE_ATTEMPT_LIMIT {
            return false;
        }
        self.human_code_attempts.push(now_ms);
        true
    }

    fn claim_offer_record(
        &mut self,
        offer_id: &str,
        public_key: DevicePublicKeyJwk,
        label: &str,
        session_expires_at_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<ClaimedDevice, PairingError> {
        self.prune(now_ms);
        let offer = self
            .offers
            .get_mut(offer_id)
            .ok_or(PairingError::NotFound)?;
        if offer.consumed {
            return Err(PairingError::AlreadyConsumed);
        }
        if now_ms >= offer.expires_at_ms {
            return Err(PairingError::Expired);
        }
        validate_public_key(&public_key)?;
        if label.trim().is_empty() {
            return Err(PairingError::BindingMismatch);
        }
        offer.consumed = true;
        let device_id = uuid::Uuid::new_v4().to_string();
        let challenge = Challenge {
            challenge_id: uuid::Uuid::new_v4().to_string(),
            device_id: device_id.clone(),
            nonce: URL_SAFE_NO_PAD.encode(secret_bytes()),
            origin: offer.origin.clone(),
            host_installation_id: offer.host_installation_id.clone(),
            offer_id: offer_id.into(),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(OFFER_LIFETIME_MS),
        };
        self.devices.insert(
            device_id.clone(),
            DeviceRecord {
                confirmed: false,
                public_key: public_key.clone(),
                revoked: false,
                session_expires_at_ms,
            },
        );
        self.pending.insert(
            device_id.clone(),
            PendingEnrollment {
                device_id: device_id.clone(),
                label: label.trim().into(),
                public_key,
                challenge: challenge.clone(),
                session_expires_at_ms,
                model: None,
            },
        );
        self.challenges.insert(
            challenge.challenge_id.clone(),
            ChallengeRecord {
                challenge: challenge.clone(),
                consumed: false,
            },
        );
        Ok(ClaimedDevice {
            device_id,
            challenge,
        })
    }

    /// Records the hardware model a claiming device reported; it is stored
    /// with the device once the owner confirms it.
    pub fn set_pending_model(&mut self, device_id: &str, model: String) {
        if let Some(pending) = self.pending.get_mut(device_id) {
            pending.model = Some(model);
        }
    }

    pub fn pending_enrollments(&self, now_ms: u64) -> Vec<PendingEnrollment> {
        self.pending
            .values()
            .filter(|pending| now_ms < pending.challenge.expires_at_ms)
            .cloned()
            .collect()
    }

    pub fn pending_enrollment(
        &self,
        device_id: &str,
        now_ms: u64,
    ) -> Result<PendingEnrollment, PairingError> {
        let pending = self.pending.get(device_id).ok_or(PairingError::NotFound)?;
        if now_ms >= pending.challenge.expires_at_ms {
            return Err(PairingError::ChallengeExpired);
        }
        Ok(pending.clone())
    }

    /// Call only after the local owner confirms and durable device storage succeeds.
    pub fn confirm_enrollment(&mut self, device_id: &str, now_ms: u64) -> Result<(), PairingError> {
        self.pending_enrollment(device_id, now_ms)?;
        self.devices
            .get_mut(device_id)
            .ok_or(PairingError::NotFound)?
            .confirmed = true;
        self.pending.remove(device_id);
        Ok(())
    }

    pub fn cancel_offer(&mut self, offer_id: &str) -> bool {
        let removed = self.offers.remove(offer_id).is_some();
        let pending: Vec<_> = self
            .pending
            .values()
            .filter(|p| p.challenge.offer_id == offer_id)
            .map(|p| p.device_id.clone())
            .collect();
        for device_id in pending {
            self.revoke_device(&device_id);
        }
        removed
    }

    pub fn verify_challenge(
        &mut self,
        challenge_id: &str,
        signature_encoded: &str,
        now_ms: u64,
    ) -> Result<VerifiedSession, PairingError> {
        self.prune(now_ms);
        let challenge = self
            .challenges
            .get_mut(challenge_id)
            .ok_or(PairingError::ChallengeNotFound)?;
        if challenge.consumed {
            return Err(PairingError::ChallengeReplayed);
        }
        if now_ms >= challenge.challenge.expires_at_ms {
            return Err(PairingError::ChallengeExpired);
        }
        let device = self
            .devices
            .get(&challenge.challenge.device_id)
            .ok_or(PairingError::NotFound)?;
        if !device.confirmed {
            return Err(PairingError::ConfirmationRequired);
        }
        if device.revoked {
            return Err(PairingError::BindingMismatch);
        }
        if device
            .session_expires_at_ms
            .is_some_and(|expires_at_ms| now_ms >= expires_at_ms)
        {
            return Err(PairingError::SessionExpired);
        }
        let session_expires_at_ms = device
            .session_expires_at_ms
            .unwrap_or(NEVER_EXPIRES_AT_MS)
            .min(now_ms.saturating_add(60 * 60 * 1000));
        let transcript = ChallengeTranscript {
            device_id: &challenge.challenge.device_id,
            challenge_id: &challenge.challenge.challenge_id,
            nonce: &challenge.challenge.nonce,
            origin: &challenge.challenge.origin,
            host_installation_id: &challenge.challenge.host_installation_id,
            issued_at_ms: challenge.challenge.issued_at_ms,
            expires_at_ms: challenge.challenge.expires_at_ms,
        };
        verify_signature(
            &device.public_key,
            &canonical_challenge_transcript(&transcript),
            signature_encoded,
        )?;
        challenge.consumed = true;
        let token = URL_SAFE_NO_PAD.encode(secret_bytes());
        let csrf_token = URL_SAFE_NO_PAD.encode(secret_bytes());
        self.sessions.insert(
            token_hash(&token),
            SessionRecord {
                device_id: challenge.challenge.device_id.clone(),
                csrf_hash: digest(csrf_token.as_bytes()),
                expires_at_ms: session_expires_at_ms,
            },
        );
        Ok(VerifiedSession {
            device_id: challenge.challenge.device_id.clone(),
            session_token: token,
            csrf_token,
            host_installation_id: challenge.challenge.host_installation_id.clone(),
            expires_at_ms: session_expires_at_ms,
        })
    }

    pub fn issue_fresh_challenge(
        &mut self,
        device_id: &str,
        origin: &str,
        host_installation_id: &str,
        now_ms: u64,
    ) -> Result<Challenge, PairingError> {
        let device = self.devices.get(device_id).ok_or(PairingError::NotFound)?;
        if !device.confirmed {
            return Err(PairingError::ConfirmationRequired);
        }
        if device.revoked {
            return Err(PairingError::BindingMismatch);
        }
        if device
            .session_expires_at_ms
            .is_some_and(|expires_at_ms| now_ms >= expires_at_ms)
        {
            return Err(PairingError::SessionExpired);
        }
        // Anyone who knows a device id can ask for a challenge, so drop the
        // expired ones and keep only this device's newest open ones.
        self.challenges
            .retain(|_, record| now_ms < record.challenge.expires_at_ms);
        let mut open: Vec<(u64, String)> = self
            .challenges
            .iter()
            .filter(|(_, record)| !record.consumed && record.challenge.device_id == device_id)
            .map(|(id, record)| (record.challenge.issued_at_ms, id.clone()))
            .collect();
        if open.len() >= MAX_OPEN_CHALLENGES_PER_DEVICE {
            open.sort_unstable();
            for (_, id) in &open[..=open.len() - MAX_OPEN_CHALLENGES_PER_DEVICE] {
                self.challenges.remove(id);
            }
        }
        let challenge = Challenge {
            challenge_id: uuid::Uuid::new_v4().to_string(),
            device_id: device_id.to_owned(),
            nonce: URL_SAFE_NO_PAD.encode(secret_bytes()),
            origin: origin.to_owned(),
            host_installation_id: host_installation_id.to_owned(),
            offer_id: "authenticated-session".into(),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(CHALLENGE_LIFETIME_MS),
        };
        self.prune(now_ms);
        self.challenges.insert(
            challenge.challenge_id.clone(),
            ChallengeRecord {
                challenge: challenge.clone(),
                consumed: false,
            },
        );
        Ok(challenge)
    }

    /// Return a newly-created session to its pre-verification state when the
    /// durable session insert fails.
    pub fn rollback_session(&mut self, challenge_id: &str, session: &VerifiedSession) {
        self.sessions.remove(&token_hash(&session.session_token));
        if let Some(challenge) = self.challenges.get_mut(challenge_id) {
            challenge.consumed = false;
        }
    }

    pub fn verify_fresh_challenge(
        &mut self,
        challenge_id: &str,
        expected_device_id: &str,
        signature_encoded: &str,
        now_ms: u64,
    ) -> Result<(), PairingError> {
        let challenge = self
            .challenges
            .get_mut(challenge_id)
            .ok_or(PairingError::ChallengeNotFound)?;
        if challenge.consumed {
            return Err(PairingError::ChallengeReplayed);
        }
        if now_ms >= challenge.challenge.expires_at_ms {
            return Err(PairingError::ChallengeExpired);
        }
        if challenge.challenge.device_id != expected_device_id {
            return Err(PairingError::BindingMismatch);
        }
        let device = self
            .devices
            .get(expected_device_id)
            .ok_or(PairingError::NotFound)?;
        if !device.confirmed {
            return Err(PairingError::ConfirmationRequired);
        }
        if device.revoked {
            return Err(PairingError::BindingMismatch);
        }
        let transcript = ChallengeTranscript {
            device_id: &challenge.challenge.device_id,
            challenge_id: &challenge.challenge.challenge_id,
            nonce: &challenge.challenge.nonce,
            origin: &challenge.challenge.origin,
            host_installation_id: &challenge.challenge.host_installation_id,
            issued_at_ms: challenge.challenge.issued_at_ms,
            expires_at_ms: challenge.challenge.expires_at_ms,
        };
        verify_signature(
            &device.public_key,
            &canonical_challenge_transcript(&transcript),
            signature_encoded,
        )?;
        challenge.consumed = true;
        Ok(())
    }

    pub fn verify_action_signature(
        &self,
        device_id: &str,
        transcript: &ActionTranscript<'_>,
        signature_encoded: &str,
    ) -> Result<(), PairingError> {
        if transcript.device_id != device_id {
            return Err(PairingError::BindingMismatch);
        }
        let device = self.devices.get(device_id).ok_or(PairingError::NotFound)?;
        if !device.confirmed {
            return Err(PairingError::ConfirmationRequired);
        }
        if device.revoked {
            return Err(PairingError::BindingMismatch);
        }
        verify_signature(
            &device.public_key,
            &canonical_action_transcript(transcript),
            signature_encoded,
        )
    }

    pub fn verify_session(
        &self,
        session_token: &str,
        csrf_token: Option<&str>,
        now_ms: u64,
    ) -> Result<String, PairingError> {
        let session = self
            .sessions
            .get(&token_hash(session_token))
            .ok_or(PairingError::SessionNotFound)?;
        let device = self
            .devices
            .get(&session.device_id)
            .ok_or(PairingError::SessionNotFound)?;
        if device.revoked || !device.confirmed {
            return Err(PairingError::SessionNotFound);
        }
        if now_ms >= session.expires_at_ms {
            return Err(PairingError::SessionExpired);
        }
        if let Some(csrf_token) = csrf_token {
            if digest(csrf_token.as_bytes())
                .ct_eq(&session.csrf_hash)
                .unwrap_u8()
                != 1
            {
                return Err(PairingError::CsrfMismatch);
            }
        }
        Ok(session.device_id.clone())
    }

    /// When the session for `session_token` stops being accepted, for streams
    /// that outlive the request that authenticated them.
    pub fn session_expires_at_ms(&self, session_token: &str) -> Option<u64> {
        self.sessions
            .get(&token_hash(session_token))
            .map(|session| session.expires_at_ms)
    }

    /// Drops a revoked device's record once the owner forgets it. Returns
    /// whether a record was removed; an unrevoked device is kept.
    pub fn forget_device(&mut self, device_id: &str) -> bool {
        if !self.devices.get(device_id).is_some_and(|d| d.revoked) {
            return false;
        }
        self.devices.remove(device_id);
        true
    }

    pub fn revoke_device(&mut self, device_id: &str) {
        self.pending.remove(device_id);
        if let Some(device) = self.devices.get_mut(device_id) {
            device.revoked = true;
        }
        self.challenges
            .retain(|_, challenge| challenge.challenge.device_id != device_id);
        self.sessions
            .retain(|_, session| session.device_id != device_id);
    }
}

pub fn session_expiration_deadline(option: &str, now_ms: u64) -> Result<Option<u64>, PairingError> {
    let lifetime_ms = match option {
        "1h" => Some(60 * 60 * 1000),
        "1d" => Some(24 * 60 * 60 * 1000),
        "7d" => Some(7 * 24 * 60 * 60 * 1000),
        "30d" => Some(30 * 24 * 60 * 60 * 1000),
        "never" => None,
        _ => return Err(PairingError::InvalidSessionExpiration),
    };
    Ok(lifetime_ms.map(|lifetime| now_ms.saturating_add(lifetime)))
}

/// The daemon's long-lived P-256 identity. A phone pins the SHA-256 of its
/// SubjectPublicKeyInfo when it pairs and checks a [`HostProof`] on every route.
pub struct HostKey {
    signing_key: SigningKey,
}

/// A host key signature over `wonder-host-proof-v1\n<origin>\n<nonce>`, with the
/// public key it was made with. Both fields are base64url without padding: the
/// key as SPKI DER, the signature as the 64-byte `r || s` of ECDSA P-256/SHA-256.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostProof {
    pub host_key: String,
    pub signature: String,
}

#[derive(Debug, Eq, PartialEq)]
pub enum HostProofError {
    InvalidHostKey,
    InvalidOrigin,
    InvalidNonce,
    PinMismatch,
    InvalidSignature,
}

impl HostKey {
    pub fn generate() -> Self {
        loop {
            // A random 32-byte string is a valid scalar except with negligible probability.
            if let Ok(signing_key) = SigningKey::from_slice(&secret_bytes()) {
                return Self { signing_key };
            }
        }
    }

    pub fn from_secret_bytes(bytes: &[u8]) -> Result<Self, HostProofError> {
        if bytes.len() != 32 {
            return Err(HostProofError::InvalidHostKey);
        }
        SigningKey::from_slice(bytes)
            .map(|signing_key| Self { signing_key })
            .map_err(|_| HostProofError::InvalidHostKey)
    }

    /// The 32-byte private scalar, for the daemon's 0600 key file.
    pub fn to_secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes().into()
    }

    pub fn public_key_spki(&self) -> Vec<u8> {
        let point = self.signing_key.verifying_key().to_sec1_bytes();
        let mut spki = Vec::with_capacity(P256_SPKI_PREFIX.len() + point.len());
        spki.extend_from_slice(&P256_SPKI_PREFIX);
        spki.extend_from_slice(&point);
        spki
    }

    /// The pin a phone saves at pairing: base64url SHA-256 of the SPKI DER.
    pub fn pin(&self) -> String {
        host_key_pin(&self.public_key_spki())
    }

    pub fn prove(&self, origin: &str, nonce: &str) -> Result<HostProof, HostProofError> {
        let transcript = host_proof_transcript(origin, nonce)?;
        let signature: Signature = self.signing_key.sign(transcript.as_bytes());
        Ok(HostProof {
            host_key: URL_SAFE_NO_PAD.encode(self.public_key_spki()),
            signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
        })
    }
}

impl std::fmt::Debug for HostKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostKey")
            .field("pin", &self.pin())
            .finish_non_exhaustive()
    }
}

pub fn host_key_pin(spki: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(digest(spki))
}

/// The signed text. The nonce is base64url and the origin has no control
/// characters, so neither can forge a line of the transcript.
pub fn host_proof_transcript(origin: &str, nonce: &str) -> Result<String, HostProofError> {
    if origin.is_empty()
        || origin.len() > HOST_PROOF_ORIGIN_MAX_LEN
        || origin.chars().any(char::is_control)
    {
        return Err(HostProofError::InvalidOrigin);
    }
    if !(HOST_PROOF_NONCE_MIN_LEN..=HOST_PROOF_NONCE_MAX_LEN).contains(&nonce.len())
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(HostProofError::InvalidNonce);
    }
    Ok(format!("{HOST_PROOF_DOMAIN}\n{origin}\n{nonce}"))
}

/// Checks that `proof` was made by the key the phone pinned at pairing, for this
/// origin and nonce.
pub fn verify_host_proof(
    pin: &str,
    origin: &str,
    nonce: &str,
    proof: &HostProof,
) -> Result<(), HostProofError> {
    let transcript = host_proof_transcript(origin, nonce)?;
    let spki = URL_SAFE_NO_PAD
        .decode(&proof.host_key)
        .map_err(|_| HostProofError::InvalidHostKey)?;
    if spki.len() != P256_SPKI_PREFIX.len() + 65
        || spki[..P256_SPKI_PREFIX.len()] != P256_SPKI_PREFIX
    {
        return Err(HostProofError::InvalidHostKey);
    }
    if host_key_pin(&spki)
        .as_bytes()
        .ct_eq(pin.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(HostProofError::PinMismatch);
    }
    let verifying_key = VerifyingKey::from_sec1_bytes(&spki[P256_SPKI_PREFIX.len()..])
        .map_err(|_| HostProofError::InvalidHostKey)?;
    let signature = URL_SAFE_NO_PAD
        .decode(&proof.signature)
        .map_err(|_| HostProofError::InvalidSignature)?;
    let signature =
        Signature::from_slice(&signature).map_err(|_| HostProofError::InvalidSignature)?;
    verifying_key
        .verify(transcript.as_bytes(), &signature)
        .map_err(|_| HostProofError::InvalidSignature)
}

fn validate_public_key(public_key: &DevicePublicKeyJwk) -> Result<(), PairingError> {
    if public_key.kty != "EC" || public_key.crv != "P-256" {
        return Err(PairingError::InvalidPublicKey);
    }
    let _ = verifying_key(public_key)?;
    Ok(())
}

fn verify_signature(
    public_key: &DevicePublicKeyJwk,
    message: &[u8],
    signature_encoded: &str,
) -> Result<(), PairingError> {
    let signature = URL_SAFE_NO_PAD
        .decode(signature_encoded)
        .map_err(|_| PairingError::InvalidSignature)?;
    let signature =
        Signature::from_slice(&signature).map_err(|_| PairingError::InvalidSignature)?;
    verifying_key(public_key)?
        .verify(message, &signature)
        .map_err(|_| PairingError::InvalidSignature)
}

fn verifying_key(public_key: &DevicePublicKeyJwk) -> Result<VerifyingKey, PairingError> {
    let x = URL_SAFE_NO_PAD
        .decode(&public_key.x)
        .map_err(|_| PairingError::InvalidPublicKey)?;
    let y = URL_SAFE_NO_PAD
        .decode(&public_key.y)
        .map_err(|_| PairingError::InvalidPublicKey)?;
    if x.len() != 32 || y.len() != 32 {
        return Err(PairingError::InvalidPublicKey);
    }
    let mut point = [0_u8; 65];
    point[0] = 4;
    point[1..33].copy_from_slice(&x);
    point[33..].copy_from_slice(&y);
    VerifyingKey::from_sec1_bytes(&point).map_err(|_| PairingError::InvalidPublicKey)
}

fn secret_bytes() -> [u8; 32] {
    let mut output = [0_u8; 32];
    getrandom::fill(&mut output).expect("operating system randomness unavailable");
    output
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn token_hash(token: &str) -> String {
    hex::encode(digest(token.as_bytes()))
}

pub fn session_token_hash(token: &str) -> String {
    token_hash(token)
}

pub fn csrf_token_hash(token: &str) -> String {
    hex::encode(digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_key_fixture() -> DevicePublicKeyJwk {
        DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: URL_SAFE_NO_PAD.encode(
                hex::decode("6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296")
                    .expect("generator x"),
            ),
            y: URL_SAFE_NO_PAD.encode(
                hex::decode("4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5")
                    .expect("generator y"),
            ),
        }
    }

    #[test]
    fn offer_is_fragment_only_and_single_use() {
        let mut state = PairingState::default();
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        assert!(offer.url.contains("#secret="));
        assert!(!offer.url.contains("?secret="));
        assert_eq!(
            state.claim_offer_with_session_expiration(
                &offer.offer_id,
                "not-the-secret",
                public_key_fixture(),
                "Chrome",
                None,
                1_001
            ),
            Err(PairingError::InvalidSecret)
        );
        let secret = offer
            .url
            .split("secret=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("secret in fragment");
        let claimed = state.claim_offer_with_session_expiration(
            &offer.offer_id,
            secret,
            public_key_fixture(),
            "Chrome",
            None,
            1_001,
        );
        let claimed = claimed.expect("valid offer claim");
        assert_eq!(claimed.challenge.device_id, claimed.device_id);
        assert_eq!(
            state.claim_offer_with_session_expiration(
                &offer.offer_id,
                secret,
                public_key_fixture(),
                "Chrome",
                None,
                1_002
            ),
            Err(PairingError::AlreadyConsumed)
        );
    }

    fn fragment_field<'a>(url: &'a str, name: &str) -> Option<&'a str> {
        url.split_once('#')?
            .1
            .split('&')
            .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
    }

    #[test]
    fn host_proof_verifies_against_the_pairing_link_pin() {
        let host_key = HostKey::from_secret_bytes(&[7_u8; 32]).expect("host fixture");
        let mut state = PairingState::default();
        let offer = state.create_offer_with_host_key(
            "https://wonder.example.ts.net",
            "install-1",
            Some(&host_key.pin()),
            1_000,
        );
        let pin = fragment_field(&offer.url, "hostKey").expect("pin in fragment");
        let origin = "https://192.168.1.20:3778";
        let nonce = URL_SAFE_NO_PAD.encode([9_u8; 32]);
        let proof = host_key.prove(origin, &nonce).expect("proof");
        let wire = serde_json::to_value(&proof).expect("proof json");
        assert_eq!(
            wire.as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>()),
            Some(vec!["hostKey".to_string(), "signature".to_string()])
        );
        let proof: HostProof = serde_json::from_value(wire).expect("proof from json");
        assert_eq!(verify_host_proof(pin, origin, &nonce, &proof), Ok(()));

        // A different host answering at the same address, with its own key or
        // presenting the pinned key it cannot sign for.
        let impostor = HostKey::from_secret_bytes(&[8_u8; 32]).expect("impostor fixture");
        let swapped = impostor.prove(origin, &nonce).expect("impostor proof");
        assert_eq!(
            verify_host_proof(pin, origin, &nonce, &swapped),
            Err(HostProofError::PinMismatch)
        );
        let borrowed = HostProof {
            host_key: proof.host_key.clone(),
            signature: swapped.signature,
        };
        assert_eq!(
            verify_host_proof(pin, origin, &nonce, &borrowed),
            Err(HostProofError::InvalidSignature)
        );
        // A proof replayed for another route or another challenge.
        assert_eq!(
            verify_host_proof(pin, "https://relay.example", &nonce, &proof),
            Err(HostProofError::InvalidSignature)
        );
        let other_nonce = URL_SAFE_NO_PAD.encode([8_u8; 32]);
        assert_eq!(
            verify_host_proof(pin, origin, &other_nonce, &proof),
            Err(HostProofError::InvalidSignature)
        );
    }

    #[test]
    fn host_key_pin_matches_an_independent_spki_hash_and_refuses_invalid_secrets() {
        // Pin computed by OpenSSL 3.0.13 from the same private scalar.
        let host_key = HostKey::from_secret_bytes(&[7_u8; 32]).expect("valid scalar");
        assert_eq!(
            host_key.pin(),
            "zpfsINQ2YaGbIVyy43ammNC0qiqltMeFTpFr0FCC_O4"
        );
        assert_eq!(
            HostKey::from_secret_bytes(&[0_u8; 32]).map(|key| key.pin()),
            Err(HostProofError::InvalidHostKey)
        );
        assert_eq!(
            HostKey::from_secret_bytes(&[7_u8; 31]).map(|key| key.pin()),
            Err(HostProofError::InvalidHostKey)
        );
    }

    // Contract: independently generated credentials must not share a secret,
    // and persisting a generated host key must preserve its identity. Check
    // freshness first: a constant zero source otherwise stalls scalar retry.
    #[test]
    fn generated_host_keys_use_fresh_secrets_and_survive_storage() {
        assert_ne!(
            secret_bytes(),
            secret_bytes(),
            "fresh credentials need fresh secrets"
        );
        let generated = HostKey::generate();
        let restored =
            HostKey::from_secret_bytes(&generated.to_secret_bytes()).expect("stored key");
        assert_eq!(restored.pin(), generated.pin());
    }

    // Contract: key diagnostics identify the public pin needed to investigate
    // a trust failure, while never exposing the stored signing secret.
    #[test]
    fn host_key_debug_identifies_the_pin_without_disclosing_the_secret() {
        let key = HostKey::from_secret_bytes(&[7_u8; 32]).expect("host fixture");
        let diagnostic = format!("{key:?}");
        assert!(diagnostic.contains(&key.pin()));
        assert!(!diagnostic.contains(&hex::encode(key.to_secret_bytes())));
    }

    // Contract: signed fields are bounded UTF-8 bytes, not characters. A
    // nonce accepts every base64url character but cannot inject another line.
    #[test]
    fn host_proof_transcript_enforces_origin_and_nonce_boundaries() {
        for origin in [
            "https://a.example".to_string(),
            "a".repeat(2048),
            "é".repeat(1024),
        ] {
            for nonce in ["A".repeat(16), "A".repeat(128), "Az09_-".repeat(3)] {
                assert_eq!(
                    host_proof_transcript(&origin, &nonce),
                    Ok(format!("wonder-host-proof-v1\n{origin}\n{nonce}"))
                );
            }
        }
        let nonce = "A".repeat(16);
        for origin in [
            String::new(),
            "a".repeat(2049),
            format!("{}a", "é".repeat(1024)),
            "https://a.example\nhttps://b.example".to_string(),
            "https://a.example\0".to_string(),
        ] {
            assert_eq!(
                host_proof_transcript(&origin, &nonce),
                Err(HostProofError::InvalidOrigin)
            );
        }
        for nonce in [
            "A".repeat(15),
            "A".repeat(129),
            "AAAAAAAAAAAAAAA!".to_string(),
            "AAAAAAAAAAAAAAA\n".to_string(),
            "AAAAAAAAAAAAAAAé".to_string(),
        ] {
            assert_eq!(
                host_proof_transcript("https://a.example", &nonce),
                Err(HostProofError::InvalidNonce)
            );
        }
    }

    // Contract: even an otherwise valid key/signature with a matching pin is
    // refused when its SPKI envelope is malformed or names another key type.
    #[test]
    fn host_proofs_refuse_malformed_public_key_envelopes() {
        let key = HostKey::from_secret_bytes(&[7_u8; 32]).expect("host fixture");
        let origin = "https://a.example";
        let nonce = "A".repeat(16);
        let proof = key.prove(origin, &nonce).expect("valid proof");
        let spki = key.public_key_spki();
        let mut wrong_header = spki.clone();
        wrong_header[0] ^= 1;
        for malformed in [
            vec![],
            spki[..25].to_vec(),
            spki[..90].to_vec(),
            wrong_header,
        ] {
            let pin = host_key_pin(&malformed);
            let malformed_proof = HostProof {
                host_key: URL_SAFE_NO_PAD.encode(&malformed),
                ..proof.clone()
            };
            assert_eq!(
                verify_host_proof(&pin, origin, &nonce, &malformed_proof),
                Err(HostProofError::InvalidHostKey)
            );
        }
    }

    #[test]
    fn offer_link_names_the_host_key_only_when_given() {
        // Current phones accept exactly three fragment fields.
        let mut state = PairingState::default();
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let fragment = offer.url.split_once('#').expect("fragment").1;
        assert_eq!(fragment.split('&').count(), 3);
        assert_eq!(fragment_field(&offer.url, "hostKey"), None);
    }

    #[test]
    fn expired_offer_cannot_be_claimed() {
        let mut state = PairingState::default();
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let secret = offer
            .url
            .split("secret=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("secret in fragment");
        assert_eq!(
            state.claim_offer_with_session_expiration(
                &offer.offer_id,
                secret,
                public_key_fixture(),
                "Chrome",
                None,
                offer.expires_at_ms
            ),
            Err(PairingError::Expired)
        );
    }

    #[test]
    fn offer_cannot_be_claimed_after_daemon_restart() {
        let mut state_before_restart = PairingState::default();
        let offer =
            state_before_restart.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let secret = offer
            .url
            .split("secret=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("secret in fragment");

        // Pairing offers are intentionally held in memory, so a fresh state
        // created during daemon startup must not accept an old offer.
        let mut state_after_restart = PairingState::default();
        assert_eq!(
            state_after_restart.claim_offer_with_session_expiration(
                &offer.offer_id,
                secret,
                public_key_fixture(),
                "Chrome",
                None,
                1_001,
            ),
            Err(PairingError::NotFound)
        );
    }

    #[test]
    fn human_code_claim_is_case_insensitive_and_single_use() {
        let mut state = PairingState::default();
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let claimed = state
            .claim_offer_by_human_code(
                &offer.human_code.to_ascii_lowercase(),
                public_key_fixture(),
                "Native iPhone",
                None,
                1_001,
            )
            .expect("valid human code claim");
        assert_eq!(claimed.challenge.offer_id, offer.offer_id);
        assert_eq!(
            state.claim_offer_by_human_code(
                &offer.human_code,
                public_key_fixture(),
                "Native iPhone",
                None,
                1_002,
            ),
            Err(PairingError::AlreadyConsumed)
        );
    }

    #[test]
    fn signed_challenge_is_verified_and_session_replay_is_rejected() {
        let mut state = PairingState::default();
        let signing_key = SigningKey::from_slice(&[1_u8; 32]).expect("deterministic signing key");
        let point = signing_key.verifying_key().to_sec1_point(false);
        let public_key = DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: URL_SAFE_NO_PAD.encode(point.x().expect("x coordinate")),
            y: URL_SAFE_NO_PAD.encode(point.y().expect("y coordinate")),
        };
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let secret = offer
            .url
            .split("secret=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("secret in fragment");
        let claimed = state
            .claim_offer_with_session_expiration(
                &offer.offer_id,
                secret,
                public_key,
                "Chrome",
                None,
                1_001,
            )
            .expect("claim");
        assert_eq!(
            state.verify_challenge(&claimed.challenge.challenge_id, "invalid", 1_001),
            Err(PairingError::ConfirmationRequired)
        );
        assert_eq!(
            state.issue_fresh_challenge(
                &claimed.device_id,
                "https://wonder.example.ts.net",
                "install-1",
                1_001
            ),
            Err(PairingError::ConfirmationRequired)
        );
        state
            .confirm_enrollment(&claimed.device_id, 1_001)
            .expect("owner confirmation");
        let transcript = ChallengeTranscript {
            device_id: &claimed.device_id,
            challenge_id: &claimed.challenge.challenge_id,
            nonce: &claimed.challenge.nonce,
            origin: &claimed.challenge.origin,
            host_installation_id: &claimed.challenge.host_installation_id,
            issued_at_ms: claimed.challenge.issued_at_ms,
            expires_at_ms: claimed.challenge.expires_at_ms,
        };
        let signature: Signature = signing_key.sign(&transcript.to_bytes());
        let signature = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        assert_eq!(
            state.verify_challenge(&claimed.challenge.challenge_id, "invalid", 1_002),
            Err(PairingError::InvalidSignature)
        );
        let session = state
            .verify_challenge(&claimed.challenge.challenge_id, &signature, 1_002)
            .expect("session");
        assert_eq!(
            state.verify_session(&session.session_token, Some(&session.csrf_token), 1_003),
            Ok(claimed.device_id)
        );
        assert_eq!(
            state.verify_challenge(&claimed.challenge.challenge_id, &signature, 1_004),
            Err(PairingError::ChallengeReplayed)
        );
    }

    #[test]
    fn an_existing_device_can_establish_a_fresh_session() {
        let mut state = PairingState::default();
        let signing_key = SigningKey::from_slice(&[2_u8; 32]).expect("deterministic signing key");
        let point = signing_key.verifying_key().to_sec1_point(false);
        let public_key = DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: URL_SAFE_NO_PAD.encode(point.x().expect("x coordinate")),
            y: URL_SAFE_NO_PAD.encode(point.y().expect("y coordinate")),
        };
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let secret = offer
            .url
            .split("secret=")
            .nth(1)
            .and_then(|value| value.split('&').next())
            .expect("secret in fragment");
        let claimed = state
            .claim_offer_with_session_expiration(
                &offer.offer_id,
                secret,
                public_key,
                "Chrome",
                None,
                1_001,
            )
            .expect("claim");
        state
            .confirm_enrollment(&claimed.device_id, 1_001)
            .expect("owner confirmation");
        let sign_challenge = |challenge: &Challenge| {
            let transcript = ChallengeTranscript {
                device_id: &challenge.device_id,
                challenge_id: &challenge.challenge_id,
                nonce: &challenge.nonce,
                origin: &challenge.origin,
                host_installation_id: &challenge.host_installation_id,
                issued_at_ms: challenge.issued_at_ms,
                expires_at_ms: challenge.expires_at_ms,
            };
            let signature: Signature = signing_key.sign(&transcript.to_bytes());
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        };
        let session = state
            .verify_challenge(
                &claimed.challenge.challenge_id,
                &sign_challenge(&claimed.challenge),
                1_002,
            )
            .expect("initial session");
        let renewed_at = session.expires_at_ms + 1;
        assert_eq!(
            state.verify_session(&session.session_token, None, renewed_at),
            Err(PairingError::SessionExpired)
        );
        let fresh = state
            .issue_fresh_challenge(
                &claimed.device_id,
                "https://wonder.example.ts.net",
                "install-1",
                renewed_at,
            )
            .expect("fresh challenge");
        let refreshed = state
            .verify_challenge(&fresh.challenge_id, &sign_challenge(&fresh), renewed_at + 1)
            .expect("refreshed session");
        assert_ne!(session.session_token, refreshed.session_token);
        assert_eq!(refreshed.device_id, claimed.device_id);
        assert_eq!(
            state.verify_session(
                &refreshed.session_token,
                Some(&refreshed.csrf_token),
                renewed_at + 2
            ),
            Ok(claimed.device_id.clone())
        );
        state.revoke_device(&claimed.device_id);
        assert_eq!(
            state.verify_session(&session.session_token, None, renewed_at + 3),
            Err(PairingError::SessionNotFound)
        );
        assert_eq!(
            state.verify_session(&refreshed.session_token, None, renewed_at + 3),
            Err(PairingError::SessionNotFound)
        );
        assert_eq!(
            state.issue_fresh_challenge(
                &claimed.device_id,
                "https://wonder.example.ts.net",
                "install-1",
                renewed_at + 3
            ),
            Err(PairingError::BindingMismatch)
        );
    }
    /// `refresh-challenge` is unauthenticated (it takes only a device id), so
    /// the challenges it leaves behind must not grow without bound: expired
    /// ones go, and each device keeps only its newest open ones.
    #[test]
    fn unanswered_fresh_challenges_stay_bounded() {
        let mut state = PairingState::default();
        let signing_key = SigningKey::from_slice(&[3_u8; 32]).expect("deterministic signing key");
        let point = signing_key.verifying_key().to_sec1_point(false);
        let public_key = DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: URL_SAFE_NO_PAD.encode(point.x().expect("x coordinate")),
            y: URL_SAFE_NO_PAD.encode(point.y().expect("y coordinate")),
        };
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let claimed = state
            .claim_offer_by_human_code(&offer.human_code, public_key, "Phone", None, 1_001)
            .unwrap();
        state.confirm_enrollment(&claimed.device_id, 1_002).unwrap();
        let sign_challenge = |challenge: &Challenge| {
            let transcript = ChallengeTranscript {
                device_id: &challenge.device_id,
                challenge_id: &challenge.challenge_id,
                nonce: &challenge.nonce,
                origin: &challenge.origin,
                host_installation_id: &challenge.host_installation_id,
                issued_at_ms: challenge.issued_at_ms,
                expires_at_ms: challenge.expires_at_ms,
            };
            let signature: Signature = signing_key.sign(&transcript.to_bytes());
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        };
        let issue = |state: &mut PairingState, now_ms| {
            state
                .issue_fresh_challenge(
                    &claimed.device_id,
                    "https://wonder.example.ts.net",
                    "install-1",
                    now_ms,
                )
                .unwrap()
        };
        let first = issue(&mut state, 2_000);
        for _ in 0..10_000 {
            issue(&mut state, 2_000);
        }
        assert!(
            state.challenges.len() <= MAX_OPEN_CHALLENGES_PER_DEVICE + 1,
            "{} challenges held",
            state.challenges.len()
        );
        // The newest still verifies; the oldest was dropped.
        let newest = issue(&mut state, 2_001);
        assert!(state
            .verify_challenge(&newest.challenge_id, &sign_challenge(&newest), 2_002)
            .is_ok());
        assert!(state
            .verify_challenge(&first.challenge_id, &sign_challenge(&first), 2_002)
            .is_err());
        // Once they expire, a later request clears them.
        issue(&mut state, 2_001 + CHALLENGE_LIFETIME_MS);
        assert!(state.challenges.len() <= 2, "{}", state.challenges.len());
    }

    #[test]
    fn reconnects_and_pairing_churn_do_not_accumulate_pairing_state() {
        let mut state = PairingState::default();
        let origin = "https://wonder.example.ts.net";
        state.restore_device("phone".into(), public_key_fixture(), false, None);
        let mut now = 1_000;
        for attempt in 0..200 {
            // An event-socket or RPC reconnect issues a fresh challenge each time.
            state
                .issue_fresh_challenge("phone", origin, "install-1", now)
                .expect("fresh challenge");
            // Offers expire unclaimed, or are claimed and then rejected or
            // never confirmed.
            let offer = state.create_offer(origin, "install-1", now);
            if attempt % 2 == 0 {
                let claimed = state
                    .claim_offer_by_human_code(
                        &offer.human_code,
                        public_key_fixture(),
                        "Phone",
                        None,
                        now,
                    )
                    .expect("claim");
                if attempt % 4 == 0 {
                    state.revoke_device(&claimed.device_id);
                }
            }
            state.restore_session(
                format!("session-{now}"),
                "phone".into(),
                [0; 32],
                now + 60 * 60 * 1000,
            );
            now += 30_000;
        }
        // Only records created within their lifetime plus the grace remain.
        let created_within =
            |lifetime_ms: u64| ((lifetime_ms + PRUNE_AFTER_EXPIRY_MS) / 30_000 + 1) as usize;
        let offer_window = created_within(OFFER_LIFETIME_MS);
        assert!(state.challenges.len() <= created_within(CHALLENGE_LIFETIME_MS) + offer_window);
        assert!(state.offers.len() <= offer_window);
        assert!(state.pending.len() <= offer_window);
        assert!(state.devices.len() <= offer_window + 1);
        assert!(state.sessions.len() <= created_within(60 * 60 * 1000));

        // A just-used challenge still reports a replay rather than vanishing.
        let challenge = state
            .issue_fresh_challenge("phone", origin, "install-1", now)
            .expect("fresh challenge");
        state
            .challenges
            .get_mut(&challenge.challenge_id)
            .unwrap()
            .consumed = true;
        assert_eq!(
            state.verify_fresh_challenge(&challenge.challenge_id, "phone", "sig", now + 1),
            Err(PairingError::ChallengeReplayed)
        );

        // Forgetting a revoked device drops its record; an active one stays.
        assert!(!state.forget_device("phone"));
        state.revoke_device("phone");
        assert!(state.forget_device("phone"));
        assert!(!state.devices.contains_key("phone"));
    }

    #[test]
    fn rejected_expired_and_cancelled_enrollments_cannot_be_confirmed() {
        for mode in ["reject", "expire", "cancel"] {
            let mut state = PairingState::default();
            let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
            let claimed = state
                .claim_offer_by_human_code(
                    &offer.human_code,
                    public_key_fixture(),
                    "Phone",
                    None,
                    1_001,
                )
                .unwrap();
            assert_eq!(state.pending_enrollments(1_002).len(), 1);
            match mode {
                "reject" => state.revoke_device(&claimed.device_id),
                "cancel" => {
                    assert!(state.cancel_offer(&offer.offer_id));
                }
                _ => {}
            }
            let now = if mode == "expire" {
                claimed.challenge.expires_at_ms
            } else {
                1_003
            };
            assert!(state.confirm_enrollment(&claimed.device_id, now).is_err());
            assert!(state.pending_enrollments(now).is_empty());
            assert!(state
                .issue_fresh_challenge(
                    &claimed.device_id,
                    "https://wonder.example.ts.net",
                    "install-1",
                    now
                )
                .is_err());
        }
    }

    fn signing_fixture(seed: u8) -> (SigningKey, DevicePublicKeyJwk) {
        let signing_key = SigningKey::from_slice(&[seed; 32]).expect("deterministic signing key");
        let point = signing_key.verifying_key().to_sec1_point(false);
        let public_key = DevicePublicKeyJwk {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: URL_SAFE_NO_PAD.encode(point.x().expect("x coordinate")),
            y: URL_SAFE_NO_PAD.encode(point.y().expect("y coordinate")),
        };
        (signing_key, public_key)
    }

    fn sign_challenge(signing_key: &SigningKey, challenge: &Challenge) -> String {
        let transcript = ChallengeTranscript {
            device_id: &challenge.device_id,
            challenge_id: &challenge.challenge_id,
            nonce: &challenge.nonce,
            origin: &challenge.origin,
            host_installation_id: &challenge.host_installation_id,
            issued_at_ms: challenge.issued_at_ms,
            expires_at_ms: challenge.expires_at_ms,
        };
        let signature: Signature = signing_key.sign(&transcript.to_bytes());
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }

    /// Claims an offer with `seed`'s key and expiry, then confirms it as the owner would.
    fn confirmed_device(
        state: &mut PairingState,
        seed: u8,
        session_expires_at_ms: Option<u64>,
    ) -> (SigningKey, ClaimedDevice) {
        let (signing_key, public_key) = signing_fixture(seed);
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let claimed = state
            .claim_offer_by_human_code(
                &offer.human_code,
                public_key,
                "Phone",
                session_expires_at_ms,
                1_001,
            )
            .expect("claim");
        state
            .confirm_enrollment(&claimed.device_id, 1_001)
            .expect("owner confirmation");
        (signing_key, claimed)
    }

    fn fresh_challenge(state: &mut PairingState, device_id: &str, now_ms: u64) -> Challenge {
        state
            .issue_fresh_challenge(
                device_id,
                "https://wonder.example.ts.net",
                "install-1",
                now_ms,
            )
            .expect("fresh challenge")
    }

    #[test]
    fn offers_and_challenges_last_their_documented_lifetimes() {
        let mut state = PairingState::default();
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        assert_eq!(offer.expires_at_ms, 1_000 + 5 * 60 * 1000);
        let (_, public_key) = signing_fixture(3);
        let claimed = state
            .claim_offer_by_human_code(
                &offer.human_code,
                public_key,
                "Phone",
                None,
                offer.expires_at_ms - 1,
            )
            .expect("an offer is claimable until its last millisecond");
        state
            .confirm_enrollment(&claimed.device_id, offer.expires_at_ms)
            .expect("owner confirmation");
        let challenge = fresh_challenge(&mut state, &claimed.device_id, 10_000);
        assert_eq!(challenge.expires_at_ms, 10_000 + 60 * 1000);
    }

    #[test]
    fn human_code_attempt_window_is_one_minute_to_the_millisecond() {
        let mut state = PairingState::default();
        for _ in 0..20 {
            assert!(state.allow_human_code_attempt(1_000));
        }
        assert!(!state.allow_human_code_attempt(60_999));
        assert!(state.allow_human_code_attempt(61_000));
    }

    #[test]
    fn every_session_expiration_option_maps_to_its_lifetime() {
        let hour = 60 * 60 * 1000;
        for (option, lifetime) in [
            ("1h", Some(hour)),
            ("1d", Some(24 * hour)),
            ("7d", Some(7 * 24 * hour)),
            ("30d", Some(30 * 24 * hour)),
            ("never", None),
        ] {
            assert_eq!(
                session_expiration_deadline(option, 1_000),
                Ok(lifetime.map(|lifetime: u64| 1_000 + lifetime)),
                "{option}"
            );
        }
        assert_eq!(
            session_expiration_deadline("90d", 1_000),
            Err(PairingError::InvalidSessionExpiration)
        );
    }

    #[test]
    fn claims_refuse_keys_that_are_not_p256_points() {
        let (_, valid) = signing_fixture(4);
        let short = URL_SAFE_NO_PAD.encode([7_u8; 31]);
        for key in [
            DevicePublicKeyJwk {
                kty: "RSA".into(),
                ..valid.clone()
            },
            DevicePublicKeyJwk {
                crv: "P-384".into(),
                ..valid.clone()
            },
            DevicePublicKeyJwk {
                x: short.clone(),
                ..valid.clone()
            },
            DevicePublicKeyJwk {
                y: short.clone(),
                ..valid.clone()
            },
            DevicePublicKeyJwk {
                y: valid.x.clone(),
                ..valid.clone()
            },
        ] {
            let mut state = PairingState::default();
            let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
            assert_eq!(
                state.claim_offer_by_human_code(&offer.human_code, key, "Phone", None, 1_001),
                Err(PairingError::InvalidPublicKey)
            );
        }
    }

    #[test]
    fn a_wrong_secret_of_the_right_length_is_refused() {
        let mut state = PairingState::default();
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        assert_eq!(
            state.claim_offer_with_session_expiration(
                &offer.offer_id,
                &URL_SAFE_NO_PAD.encode([0_u8; 32]),
                public_key_fixture(),
                "Chrome",
                None,
                1_001
            ),
            Err(PairingError::InvalidSecret)
        );
    }

    #[test]
    fn a_device_session_expiry_ends_challenges_and_caps_sessions_at_an_hour() {
        let hour = 60 * 60 * 1000;
        let mut state = PairingState::default();
        let (key, never) = confirmed_device(&mut state, 5, None);
        let session = state
            .verify_challenge(
                &never.challenge.challenge_id,
                &sign_challenge(&key, &never.challenge),
                2_000,
            )
            .expect("session");
        assert_eq!(session.expires_at_ms, 2_000 + hour);
        assert_eq!(
            state.session_expires_at_ms(&session.session_token),
            Some(2_000 + hour)
        );
        assert_eq!(state.session_expires_at_ms("unknown-token"), None);
        assert_eq!(
            state.verify_session("unknown-token", None, 2_001),
            Err(PairingError::SessionNotFound)
        );
        assert_eq!(
            state.verify_session(&session.session_token, Some("wrong-csrf"), 2_001),
            Err(PairingError::CsrfMismatch)
        );

        let deadline = 10_000;
        let (key, limited) = confirmed_device(&mut state, 6, Some(deadline));
        let fresh = fresh_challenge(&mut state, &limited.device_id, deadline - 1);
        let session = state
            .verify_challenge(
                &fresh.challenge_id,
                &sign_challenge(&key, &fresh),
                deadline - 1,
            )
            .expect("session before the device's deadline");
        assert_eq!(session.expires_at_ms, deadline);
        assert_eq!(
            state.issue_fresh_challenge(
                &limited.device_id,
                "https://wonder.example.ts.net",
                "install-1",
                deadline
            ),
            Err(PairingError::SessionExpired)
        );
        assert_eq!(
            state.verify_challenge(
                &limited.challenge.challenge_id,
                &sign_challenge(&key, &limited.challenge),
                deadline
            ),
            Err(PairingError::SessionExpired)
        );
    }

    #[test]
    fn a_rolled_back_session_is_refused_and_its_challenge_can_be_answered_again() {
        let mut state = PairingState::default();
        let (key, claimed) = confirmed_device(&mut state, 7, None);
        let signature = sign_challenge(&key, &claimed.challenge);
        let session = state
            .verify_challenge(&claimed.challenge.challenge_id, &signature, 2_000)
            .expect("session");
        state.rollback_session(&claimed.challenge.challenge_id, &session);
        assert_eq!(
            state.verify_session(&session.session_token, None, 2_001),
            Err(PairingError::SessionNotFound)
        );
        state
            .verify_challenge(&claimed.challenge.challenge_id, &signature, 2_001)
            .expect("the challenge is unconsumed again");
    }

    #[test]
    fn a_fresh_challenge_proves_its_own_device_once_before_it_expires() {
        let mut state = PairingState::default();
        let (key, claimed) = confirmed_device(&mut state, 8, None);
        let (_, other) = confirmed_device(&mut state, 9, None);
        let fresh = fresh_challenge(&mut state, &claimed.device_id, 2_000);
        let signature = sign_challenge(&key, &fresh);
        assert_eq!(
            state.verify_fresh_challenge("unknown", &claimed.device_id, &signature, 2_001),
            Err(PairingError::ChallengeNotFound)
        );
        assert_eq!(
            state.verify_fresh_challenge(&fresh.challenge_id, &other.device_id, &signature, 2_001),
            Err(PairingError::BindingMismatch)
        );
        assert_eq!(
            state.verify_fresh_challenge(
                &fresh.challenge_id,
                &claimed.device_id,
                &signature,
                fresh.expires_at_ms
            ),
            Err(PairingError::ChallengeExpired)
        );
        assert_eq!(
            state.verify_fresh_challenge(&fresh.challenge_id, &claimed.device_id, "invalid", 2_001),
            Err(PairingError::InvalidSignature)
        );
        assert_eq!(
            state.verify_fresh_challenge(
                &fresh.challenge_id,
                &claimed.device_id,
                &signature,
                2_001
            ),
            Ok(())
        );
        assert_eq!(
            state.verify_fresh_challenge(
                &fresh.challenge_id,
                &claimed.device_id,
                &signature,
                2_002
            ),
            Err(PairingError::ChallengeReplayed)
        );

        let mut state = PairingState::default();
        let (key, public_key) = signing_fixture(10);
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let unconfirmed = state
            .claim_offer_by_human_code(&offer.human_code, public_key, "Phone", None, 1_001)
            .expect("claim");
        assert_eq!(
            state.verify_fresh_challenge(
                &unconfirmed.challenge.challenge_id,
                &unconfirmed.device_id,
                &sign_challenge(&key, &unconfirmed.challenge),
                1_002
            ),
            Err(PairingError::ConfirmationRequired)
        );
    }

    #[test]
    fn action_signatures_bind_the_confirmed_unrevoked_signing_device() {
        let mut state = PairingState::default();
        let (key, claimed) = confirmed_device(&mut state, 11, None);
        let (_, other) = confirmed_device(&mut state, 12, None);
        let transcript = ActionTranscript {
            action: "approval.resolve",
            target: "/api/v1/approvals/approval-1/resolve",
            body_sha256: "body-hash",
            action_nonce: "nonce-1",
            session_binding: "session-1",
            device_id: &claimed.device_id,
            host_installation_id: "install-1",
            issued_at_ms: 1_000,
            expected_state: "pending",
        };
        let signature: Signature = key.sign(&transcript.to_bytes());
        let signature = URL_SAFE_NO_PAD.encode(signature.to_bytes());
        assert_eq!(
            state.verify_action_signature(&claimed.device_id, &transcript, &signature),
            Ok(())
        );
        assert_eq!(
            state.verify_action_signature(&other.device_id, &transcript, &signature),
            Err(PairingError::BindingMismatch)
        );
        let tampered = ActionTranscript {
            expected_state: "resolved",
            ..transcript
        };
        assert_eq!(
            state.verify_action_signature(&claimed.device_id, &tampered, &signature),
            Err(PairingError::InvalidSignature)
        );

        let (key, public_key) = signing_fixture(13);
        let offer = state.create_offer("https://wonder.example.ts.net", "install-1", 1_000);
        let unconfirmed = state
            .claim_offer_by_human_code(&offer.human_code, public_key, "Phone", None, 1_001)
            .expect("claim");
        let transcript = ActionTranscript {
            device_id: &unconfirmed.device_id,
            ..transcript
        };
        let signature: Signature = key.sign(&transcript.to_bytes());
        assert_eq!(
            state.verify_action_signature(
                &unconfirmed.device_id,
                &transcript,
                &URL_SAFE_NO_PAD.encode(signature.to_bytes())
            ),
            Err(PairingError::ConfirmationRequired)
        );
    }

    #[test]
    fn revoking_a_device_ends_only_its_own_sessions_and_challenges() {
        let mut state = PairingState::default();
        let (revoked_key, revoked) = confirmed_device(&mut state, 14, None);
        let (kept_key, kept) = confirmed_device(&mut state, 15, None);
        let kept_session = state
            .verify_challenge(
                &kept.challenge.challenge_id,
                &sign_challenge(&kept_key, &kept.challenge),
                2_000,
            )
            .expect("session");
        let kept_fresh = fresh_challenge(&mut state, &kept.device_id, 2_000);
        let revoked_fresh = fresh_challenge(&mut state, &revoked.device_id, 2_000);
        state.revoke_device(&revoked.device_id);
        assert_eq!(
            state.verify_session(&kept_session.session_token, None, 2_001),
            Ok(kept.device_id.clone())
        );
        assert_eq!(
            state.verify_fresh_challenge(
                &kept_fresh.challenge_id,
                &kept.device_id,
                &sign_challenge(&kept_key, &kept_fresh),
                2_001
            ),
            Ok(())
        );
        assert_eq!(
            state.verify_fresh_challenge(
                &revoked_fresh.challenge_id,
                &revoked.device_id,
                &sign_challenge(&revoked_key, &revoked_fresh),
                2_001
            ),
            Err(PairingError::ChallengeNotFound)
        );
    }

    #[test]
    fn a_stored_session_survives_restart_until_its_device_is_revoked() {
        // The daemon stores session_token_hash and csrf_token_hash (hex) and
        // restores both at startup (wonderd main.rs).
        let restored = |revoked: bool| {
            let mut state = PairingState::default();
            state.restore_device("phone".into(), public_key_fixture(), revoked, None);
            let csrf_hash: [u8; 32] = hex::decode(csrf_token_hash("csrf-token"))
                .expect("hex CSRF hash")
                .try_into()
                .expect("32-byte CSRF hash");
            state.restore_session(
                session_token_hash("session-token"),
                "phone".into(),
                csrf_hash,
                5_000,
            );
            state
        };
        let state = restored(false);
        assert_eq!(
            state.verify_session("session-token", Some("csrf-token"), 4_999),
            Ok("phone".into())
        );
        assert_eq!(
            state.verify_session("session-token", None, 5_000),
            Err(PairingError::SessionExpired)
        );
        assert_eq!(
            restored(true).verify_session("session-token", Some("csrf-token"), 4_999),
            Err(PairingError::SessionNotFound)
        );
        // Stored hashes must stay readable across upgrades: plain SHA-256 hex.
        assert_eq!(
            session_token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(csrf_token_hash("abc"), session_token_hash("abc"));
    }
}
