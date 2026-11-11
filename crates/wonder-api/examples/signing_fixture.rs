//! Synthetic interoperability fixture. The fixed key is public test data only.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use p256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey,
};
use serde_json::json;
use wonder_api::pairing::{ActionTranscript, ChallengeTranscript};

fn main() {
    let key = SigningKey::from_slice(&[1_u8; 32]).unwrap();
    let challenge = ChallengeTranscript {
        device_id: "device-01",
        challenge_id: "challenge-01",
        nonce: "nonce-01",
        origin: "https://wonder.example.ts.net",
        host_installation_id: "install-1",
        issued_at_ms: 1_000,
        expires_at_ms: 61_000,
    }
    .to_bytes();
    let action = ActionTranscript {
        action: "approval.resolve",
        target: "/api/v1/approvals/approval-1/resolve",
        body_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        action_nonce: "nonce-1",
        session_binding: "session-1",
        device_id: "device-01",
        host_installation_id: "install-1",
        issued_at_ms: 1_000,
        expected_state: "pending",
    }
    .to_bytes();
    let transcripts = [challenge, action];
    if let Some(path) = std::env::args().nth(1) {
        let signatures: Vec<String> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(signatures.len(), transcripts.len());
        for (bytes, signature) in transcripts.iter().zip(signatures) {
            let signature =
                Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
            key.verifying_key()
                .verify(bytes, &signature)
                .expect("Swift signature verifies in Rust");
        }
        println!("Rust verified both Swift signatures");
        return;
    }
    let fixtures: Vec<_> = transcripts.iter().map(|bytes| {
        let signature: Signature = key.sign(bytes);
        json!({"transcriptHex": hex::encode(bytes), "signature": URL_SAFE_NO_PAD.encode(signature.to_bytes())})
    }).collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "fixtureVersion": 1,
            "testPrivateKeyHex": hex::encode([1_u8; 32]),
            "publicKeyX963Hex": hex::encode(key.verifying_key().to_sec1_point(false).as_bytes()),
            "fixtures": fixtures
        }))
        .unwrap()
    );
}
