//! Sealed Wonder RPC: the frames of `/api/v1/rpc/sealed`, encrypted end to end
//! between the client and this host so a relay that ends TLS (Cloudflare's
//! edge, for the relay route) sees only ciphertext.
//!
//! The handshake is two JSON text messages; everything after is binary:
//! 1. client: `{"v":"wonder-sealed-v1","clientKey":<X25519 public, base64url>}`
//! 2. host: `{"serverKey":<X25519 public>,"proof":<HostProof>}`, the host key's
//!    signature (the key the phone pinned at pairing) over the origin the
//!    client dialed and [`handshake_nonce`] of both keys.
//!
//! Both sides then derive one ChaCha20-Poly1305 key per direction with
//! HKDF-SHA256 over the X25519 secret, salted with the transcript hash. Each
//! frame's nonce is its position in that direction, so a dropped, reordered,
//! replayed or altered frame fails to open and the connection ends.
//!
//! The device authenticates inside the channel: its first sealed frame is
//! `{"session":<token>}`, then the unchanged JSON-RPC protocol runs
//! (`auth.challenge`, `initialize` with the CSRF token and a signature). The
//! session cookie therefore never crosses the relay. A device whose session
//! has ended renews it here instead ([`Renewal`]), since the relay serves no
//! other path to renew through. The phone's device key is
//! a Secure Enclave signing key that cannot do key agreement, so the device is
//! proved by signature, not by a static Diffie-Hellman key as in Noise IK.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305},
    agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519},
    hkdf,
    rand::SystemRandom,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wonder_api::pairing_protocol::{verify_host_proof, HostKey, HostProof};

pub const VERSION: &str = "wonder-sealed-v1";
const CLIENT_TO_HOST: &[u8] = b"wonder-sealed-v1 client to host";
const HOST_TO_CLIENT: &[u8] = b"wonder-sealed-v1 host to client";
/// The largest handshake message either side reads.
pub const MAX_HANDSHAKE_BYTES: usize = 4 * 1024;
pub const TAG_BYTES: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Hello {
    pub v: String,
    pub client_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Welcome {
    pub server_key: String,
    pub proof: HostProof,
}

/// The client's first sealed frame: `{"session":<token>}` with the session it
/// holds, or `{"renew":{"deviceId":<id>}}` when that session has ended and the
/// device will sign a fresh challenge for a new one (see [`Renewal`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Opening {
    Session(String),
    Renew(RenewRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewRequest {
    pub device_id: String,
}

/// Renewal, the one step allowed before a session: the host answers a
/// [`Opening::Renew`] with `{"challenge":<Challenge>}` (a one-time challenge for
/// that device and the origin this socket arrived on), the device answers with
/// [`RenewAnswer`], and the host replies `{"renewed":<RenewedSession>}`. The
/// connection then continues exactly as one opened with that session. Any
/// refusal is one sealed JSON-RPC error (`id: null`) and the socket closes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Renewal {
    Challenge(wonder_api::pairing_protocol::Challenge),
    Renewed(RenewedSession),
}

/// The device's signature over the challenge's canonical transcript, as for
/// `POST /api/v1/pairing/session`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewAnswer {
    pub challenge_id: String,
    pub signature: String,
}

/// The new session, as `POST /api/v1/pairing/session` returns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewedSession {
    pub session_token: String,
    pub csrf_token: String,
    pub device_id: String,
    pub host_installation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealedError {
    /// Not a handshake message this version reads.
    Handshake,
    /// The host proof does not come from the pinned key, for this origin.
    HostProof,
    /// A frame failed to open: altered, out of order, replayed or truncated.
    Frame,
    /// A direction ran out of nonces; open a new connection.
    Exhausted,
}

impl std::fmt::Display for SealedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Handshake => "The computer did not complete a secure connection.",
            Self::HostProof => {
                "This computer's key does not match the one from pairing. Pair it again."
            }
            Self::Frame => "The secure connection was interrupted.",
            Self::Exhausted => "The secure connection ran too long. Reconnect.",
        })
    }
}

impl std::error::Error for SealedError {}

/// The base64url SHA-256 of `wonder-sealed-v1\n‖clientKey‖serverKey`: the
/// nonce the host proof signs, and the key schedule's salt.
pub fn handshake_nonce(client_key: &[u8], server_key: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(transcript_hash(client_key, server_key))
}

fn transcript_hash(client_key: &[u8], server_key: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(VERSION.as_bytes());
    hash.update(b"\n");
    hash.update(client_key);
    hash.update(server_key);
    hash.finalize().into()
}

fn x25519_public(text: &str) -> Result<[u8; 32], SealedError> {
    URL_SAFE_NO_PAD
        .decode(text)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or(SealedError::Handshake)
}

fn ephemeral() -> Result<(EphemeralPrivateKey, [u8; 32]), SealedError> {
    let private = EphemeralPrivateKey::generate(&X25519, &SystemRandom::new())
        .map_err(|_| SealedError::Handshake)?;
    let public = private
        .compute_public_key()
        .map_err(|_| SealedError::Handshake)?;
    let public = <[u8; 32]>::try_from(public.as_ref()).map_err(|_| SealedError::Handshake)?;
    Ok((private, public))
}

/// HKDF output as a raw 32-byte ChaCha20-Poly1305 key.
struct KeyBytes;

impl hkdf::KeyType for KeyBytes {
    fn len(&self) -> usize {
        32
    }
}

/// The key schedule: `(client to host, host to client)` keys from the X25519
/// secret, salted with the transcript hash. Clients reproduce it from
/// `tests/contracts/native/sealed-v1.json`.
fn schedule(
    secret: &[u8],
    client_key: &[u8; 32],
    server_key: &[u8; 32],
) -> Result<([u8; 32], [u8; 32]), SealedError> {
    let salt = transcript_hash(client_key, server_key);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &salt).extract(secret);
    let key = |info: &[u8]| -> Result<[u8; 32], SealedError> {
        let info = [info];
        let mut bytes = [0_u8; 32];
        prk.expand(&info, KeyBytes)
            .and_then(|okm| okm.fill(&mut bytes))
            .map_err(|_| SealedError::Handshake)?;
        Ok(bytes)
    };
    Ok((key(CLIENT_TO_HOST)?, key(HOST_TO_CLIENT)?))
}

fn directions(out: &[u8; 32], inn: &[u8; 32]) -> Result<(Sealer, Opener), SealedError> {
    let key = |bytes: &[u8; 32]| {
        UnboundKey::new(&CHACHA20_POLY1305, bytes)
            .map(LessSafeKey::new)
            .map_err(|_| SealedError::Handshake)
    };
    Ok((
        Sealer {
            key: key(out)?,
            counter: 0,
        },
        Opener {
            key: key(inn)?,
            counter: 0,
        },
    ))
}

/// Derives both directions' keys; `true` for the host's view.
fn channel(
    private: EphemeralPrivateKey,
    peer: &[u8; 32],
    client_key: &[u8; 32],
    server_key: &[u8; 32],
    host: bool,
) -> Result<(Sealer, Opener), SealedError> {
    // ring refuses a peer key that yields the all-zero secret.
    agreement::agree_ephemeral(private, &UnparsedPublicKey::new(&X25519, peer), |secret| {
        let (to_host, to_client) = schedule(secret, client_key, server_key)?;
        if host {
            directions(&to_client, &to_host)
        } else {
            directions(&to_host, &to_client)
        }
    })
    .map_err(|_| SealedError::Handshake)?
}

/// The host's half of the handshake: reads the client's hello and returns the
/// welcome to send with the channel it opens.
pub fn accept(
    hello: &str,
    host_key: &HostKey,
    origin: &str,
) -> Result<(String, Sealer, Opener), SealedError> {
    if hello.len() > MAX_HANDSHAKE_BYTES {
        return Err(SealedError::Handshake);
    }
    let hello: Hello = serde_json::from_str(hello).map_err(|_| SealedError::Handshake)?;
    if hello.v != VERSION {
        return Err(SealedError::Handshake);
    }
    let client_key = x25519_public(&hello.client_key)?;
    let (private, server_key) = ephemeral()?;
    let proof = host_key
        .prove(origin, &handshake_nonce(&client_key, &server_key))
        .map_err(|_| SealedError::Handshake)?;
    let (sealer, opener) = channel(private, &client_key, &client_key, &server_key, true)?;
    let welcome = serde_json::to_string(&Welcome {
        server_key: URL_SAFE_NO_PAD.encode(server_key),
        proof,
    })
    .map_err(|_| SealedError::Handshake)?;
    Ok((welcome, sealer, opener))
}

/// The client's half: [`Initiator::hello`] to send, then [`Initiator::finish`]
/// with the host's welcome.
pub struct Initiator {
    private: EphemeralPrivateKey,
    client_key: [u8; 32],
}

impl Initiator {
    pub fn new() -> Result<Self, SealedError> {
        let (private, client_key) = ephemeral()?;
        Ok(Self {
            private,
            client_key,
        })
    }

    pub fn hello(&self) -> String {
        serde_json::to_string(&Hello {
            v: VERSION.into(),
            client_key: URL_SAFE_NO_PAD.encode(self.client_key),
        })
        .unwrap_or_default()
    }

    /// Checks the host proof against `pin` (from pairing) and the `origin`
    /// this client dialed, then opens the channel.
    pub fn finish(
        self,
        welcome: &str,
        pin: &str,
        origin: &str,
    ) -> Result<(Sealer, Opener), SealedError> {
        if welcome.len() > MAX_HANDSHAKE_BYTES {
            return Err(SealedError::Handshake);
        }
        let welcome: Welcome = serde_json::from_str(welcome).map_err(|_| SealedError::Handshake)?;
        let server_key = x25519_public(&welcome.server_key)?;
        let nonce = handshake_nonce(&self.client_key, &server_key);
        verify_host_proof(pin, origin, &nonce, &welcome.proof)
            .map_err(|_| SealedError::HostProof)?;
        channel(
            self.private,
            &server_key,
            &self.client_key,
            &server_key,
            false,
        )
    }
}

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0_u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(bytes)
}

/// Seals one direction's frames in order.
pub struct Sealer {
    key: LessSafeKey,
    counter: u64,
}

impl Sealer {
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, SealedError> {
        if self.counter == u64::MAX {
            return Err(SealedError::Exhausted);
        }
        let mut frame = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce(self.counter), Aad::empty(), &mut frame)
            .map_err(|_| SealedError::Frame)?;
        self.counter += 1;
        Ok(frame)
    }
}

/// Opens the other direction's frames, which must arrive in order.
pub struct Opener {
    key: LessSafeKey,
    counter: u64,
}

impl Opener {
    /// Opens the next frame. After an error the channel is unusable: the
    /// caller closes the connection.
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, SealedError> {
        if self.counter == u64::MAX {
            return Err(SealedError::Exhausted);
        }
        let mut plaintext = frame.to_vec();
        let length = self
            .key
            .open_in_place(nonce(self.counter), Aad::empty(), &mut plaintext)
            .map_err(|_| SealedError::Frame)?
            .len();
        plaintext.truncate(length);
        self.counter += 1;
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: &str = "https://r-k7x2q.getcupola.app";

    fn pair(host_key: &HostKey) -> ((Sealer, Opener), (Sealer, Opener)) {
        let initiator = Initiator::new().unwrap();
        let (welcome, host_sealer, host_opener) =
            accept(&initiator.hello(), host_key, ORIGIN).unwrap();
        let client = initiator.finish(&welcome, &host_key.pin(), ORIGIN).unwrap();
        (client, (host_sealer, host_opener))
    }

    /// Property: whatever arrives as a hello or a frame (the relay edge and
    /// anyone reaching it choose these bytes), the host refuses it without
    /// panicking; every one-bit change to a real frame fails to open, and the
    /// channel still opens the real frame afterwards.
    #[cfg(unix)]
    #[test]
    fn arbitrary_hellos_and_frames_are_refused_without_panicking() {
        let mut fuzz = crate::test_support::Fuzz::new(0x5EA1ED);
        let host_key = HostKey::generate();
        let valid = Initiator::new().unwrap().hello();
        for _ in 0..2_000 {
            let mut mutated = valid.clone().into_bytes();
            let at = fuzz.below(mutated.len());
            mutated[at] = fuzz.next() as u8;
            for hello in [
                fuzz.text(96),
                String::from_utf8_lossy(&mutated).into_owned(),
            ] {
                if let Ok((welcome, _, _)) = accept(&hello, &host_key, ORIGIN) {
                    // Only a well-formed hello naming a real key gets this far.
                    assert!(serde_json::from_str::<Welcome>(&welcome).is_ok());
                }
            }
        }
        let ((mut client_out, _), (_, mut host_in)) = pair(&host_key);
        for _ in 0..2_000 {
            assert!(host_in.open(&fuzz.bytes(96)).is_err());
        }
        let frame = client_out.seal(b"{\"session\":\"token\"}").unwrap();
        for bit in 0..frame.len() * 8 {
            let mut flipped = frame.clone();
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert!(host_in.open(&flipped).is_err(), "bit {bit}");
        }
        assert!(host_in.open(&frame[..frame.len() - 1]).is_err());
        assert_eq!(host_in.open(&frame).unwrap(), b"{\"session\":\"token\"}");
    }

    #[test]
    fn frames_round_trip_both_ways_and_hide_their_text() {
        let host_key = HostKey::generate();
        let ((mut client_out, mut client_in), (mut host_out, mut host_in)) = pair(&host_key);
        for text in [&b"{\"session\":\"secret-token\"}"[..], b"second", b""] {
            let frame = client_out.seal(text).unwrap();
            assert_eq!(frame.len(), text.len() + TAG_BYTES);
            if !text.is_empty() {
                assert!(!frame.windows(text.len()).any(|w| w == text));
            }
            assert_eq!(host_in.open(&frame).unwrap(), text);
        }
        let reply = host_out.seal(b"reply").unwrap();
        assert_eq!(client_in.open(&reply).unwrap(), b"reply");
        // The same text sealed twice differs: each frame has its own nonce.
        assert_ne!(
            client_out.seal(b"x").unwrap(),
            client_out.seal(b"x").unwrap()
        );
    }

    #[test]
    fn altered_reordered_replayed_or_reflected_frames_do_not_open() {
        let host_key = HostKey::generate();
        // Altered: the first byte, the last tag byte, or cut short.
        for alter in [
            |f: &mut Vec<u8>| f[0] ^= 1,
            |f: &mut Vec<u8>| *f.last_mut().unwrap() ^= 1,
            |f: &mut Vec<u8>| f.truncate(TAG_BYTES - 1),
        ] {
            let ((mut out, _), (_, mut host_in)) = pair(&host_key);
            let mut frame = out.seal(b"first").unwrap();
            alter(&mut frame);
            assert_eq!(host_in.open(&frame), Err(SealedError::Frame));
        }

        let ((mut out, _), (_, mut host_in)) = pair(&host_key);
        let _first = out.seal(b"first").unwrap();
        let second = out.seal(b"second").unwrap();
        assert_eq!(host_in.open(&second), Err(SealedError::Frame), "reordered");

        let ((mut out, _), (_, mut host_in)) = pair(&host_key);
        let once = out.seal(b"once").unwrap();
        assert_eq!(host_in.open(&once).unwrap(), b"once");
        assert_eq!(host_in.open(&once), Err(SealedError::Frame), "replayed");

        // A host frame sent back to the host does not open as the client's.
        let (_, (mut host_out, mut host_in)) = pair(&host_key);
        let reflected = host_out.seal(b"reflected").unwrap();
        assert_eq!(host_in.open(&reflected), Err(SealedError::Frame));
    }

    /// Every connection derives its own keys, so a frame recorded on one
    /// connection (even its first, at the same position) opens on no other,
    /// to the same host with the same pinned key.
    #[test]
    fn a_frame_replayed_into_another_connection_does_not_open() {
        let host_key = HostKey::generate();
        let ((mut first_out, _), _) = pair(&host_key);
        let recorded = first_out.seal(b"{\"session\":\"token\"}").unwrap();
        for _ in 0..16 {
            let (_, (_, mut host_in)) = pair(&host_key);
            assert_eq!(host_in.open(&recorded), Err(SealedError::Frame));
        }
        // And a host frame recorded once does not open for another client.
        let (_, (mut host_out, _)) = pair(&host_key);
        let reply = host_out.seal(b"reply").unwrap();
        let ((_, mut client_in), _) = pair(&host_key);
        assert_eq!(client_in.open(&reply), Err(SealedError::Frame));
    }

    #[test]
    fn an_impostor_host_or_another_origin_fails_the_pin() {
        let host_key = HostKey::generate();
        let impostor = HostKey::generate();
        let initiator = Initiator::new().unwrap();
        let (welcome, _, _) = accept(&initiator.hello(), &impostor, ORIGIN).unwrap();
        assert_eq!(
            initiator.finish(&welcome, &host_key.pin(), ORIGIN).err(),
            Some(SealedError::HostProof)
        );

        let initiator = Initiator::new().unwrap();
        let (welcome, _, _) =
            accept(&initiator.hello(), &host_key, "https://other.example").unwrap();
        assert_eq!(
            initiator.finish(&welcome, &host_key.pin(), ORIGIN).err(),
            Some(SealedError::HostProof)
        );

        // A welcome replayed into another handshake names other keys.
        let first = Initiator::new().unwrap();
        let (welcome, _, _) = accept(&first.hello(), &host_key, ORIGIN).unwrap();
        let second = Initiator::new().unwrap();
        assert_eq!(
            second.finish(&welcome, &host_key.pin(), ORIGIN).err(),
            Some(SealedError::HostProof)
        );
    }

    /// The cross-language contract: clients (the iPhone's
    /// `SealedChannelTests`) check their X25519, key schedule, frames and host
    /// proof against this file. The keys are RFC 7748 section 6.1's (client:
    /// Alice, host: Bob) and a fixed host key, which signs deterministically
    /// (RFC 6979). `WONDER_UPDATE_VECTORS=1` rewrites the file.
    #[test]
    fn the_client_test_vectors_match_this_channel() {
        let hex = |text: &str| hex::decode(text).unwrap();
        let client_private = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
        let server_private = "5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb";
        let client_key: [u8; 32] =
            hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
                .try_into()
                .unwrap();
        let server_key: [u8; 32] =
            hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
                .try_into()
                .unwrap();
        let secret = hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
        let host_secret = [0x5c_u8; 32];
        let host_key = HostKey::from_secret_bytes(&host_secret).unwrap();
        let nonce = handshake_nonce(&client_key, &server_key);
        let welcome = serde_json::to_string(&Welcome {
            server_key: URL_SAFE_NO_PAD.encode(server_key),
            proof: host_key.prove(ORIGIN, &nonce).unwrap(),
        })
        .unwrap();
        let hello = serde_json::to_string(&Hello {
            v: VERSION.into(),
            client_key: URL_SAFE_NO_PAD.encode(client_key),
        })
        .unwrap();
        verify_host_proof(
            &host_key.pin(),
            ORIGIN,
            &nonce,
            &serde_json::from_str::<Welcome>(&welcome).unwrap().proof,
        )
        .unwrap();

        let (to_host, to_client) = schedule(&secret, &client_key, &server_key).unwrap();
        let (mut client_out, _) = directions(&to_host, &to_client).unwrap();
        let (mut host_out, _) = directions(&to_client, &to_host).unwrap();
        let mut frames = Vec::new();
        for (direction, text) in [
            ("clientToHost", r#"{"session":"vector-session"}"#),
            (
                "clientToHost",
                r#"{"jsonrpc":"2.0","id":1,"method":"auth.challenge"}"#,
            ),
            ("hostToClient", r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            ("clientToHost", ""),
        ] {
            let sealer = if direction == "clientToHost" {
                &mut client_out
            } else {
                &mut host_out
            };
            let sealed = sealer.seal(text.as_bytes()).unwrap();
            frames.push(serde_json::json!({
                "direction": direction,
                "plaintext": text,
                "sealed": URL_SAFE_NO_PAD.encode(sealed),
            }));
        }
        let vector = serde_json::json!({
            "version": VERSION,
            "origin": ORIGIN,
            "otherOrigin": "https://other.example",
            "clientPrivate": client_private,
            "serverPrivate": server_private,
            "clientKey": URL_SAFE_NO_PAD.encode(client_key),
            "serverKey": URL_SAFE_NO_PAD.encode(server_key),
            "sharedSecret": hex::encode(&secret),
            "handshakeNonce": nonce,
            "hello": hello,
            "hostSecret": hex::encode(host_secret),
            "hostKeyPin": host_key.pin(),
            "welcome": welcome,
            "clientToHostKey": hex::encode(to_host),
            "hostToClientKey": hex::encode(to_client),
            "frames": frames,
        });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/contracts/native/sealed-v1.json");
        let text = serde_json::to_string_pretty(&vector).unwrap() + "\n";
        if std::env::var_os("WONDER_UPDATE_VECTORS").is_some() {
            std::fs::write(&path, &text).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            text,
            "rerun with WONDER_UPDATE_VECTORS=1 and update the clients"
        );
    }

    #[test]
    fn the_host_reads_only_this_versions_hello() {
        let host_key = HostKey::generate();
        let key = URL_SAFE_NO_PAD.encode([9_u8; 32]);
        for hello in [
            String::new(),
            "{}".into(),
            format!(r#"{{"v":"wonder-sealed-v0","clientKey":"{key}"}}"#),
            format!(r#"{{"v":"{VERSION}","clientKey":"short"}}"#),
            format!(r#"{{"v":"{VERSION}","clientKey":"{key}","extra":1}}"#),
            // The all-zero point gives an all-zero secret; ring refuses it.
            format!(
                r#"{{"v":"{VERSION}","clientKey":"{}"}}"#,
                URL_SAFE_NO_PAD.encode([0_u8; 32])
            ),
            "x".repeat(MAX_HANDSHAKE_BYTES + 1),
        ] {
            assert_eq!(
                accept(&hello, &host_key, ORIGIN).err(),
                Some(SealedError::Handshake),
                "{hello}"
            );
        }
    }
}
