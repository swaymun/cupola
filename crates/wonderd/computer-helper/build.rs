use sha2::{Digest, Sha256};
use std::{env, fs, path::PathBuf};

fn main() {
    // libwebrtc's Objective-C categories live in a static library; without -ObjC the
    // test-only macOS answerer dies at load ("unrecognized selector stringForStdString:").
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bin=computer-parity-receiver=-ObjC");
    }
    println!("cargo:rerun-if-env-changed=LK_CUSTOM_WEBRTC");
    let binding = match env::var_os("LK_CUSTOM_WEBRTC") {
        None => "prebuilt".to_owned(),
        Some(root) => {
            let root = PathBuf::from(root);
            if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
                && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
            {
                assert!(std::process::Command::new("pkg-config").args(["--exists", "libva"])
                    .status().is_ok_and(|s| s.success()),
                    "Release H264 requires VAAPI headers: install libva-dev before building the helper");
            }
            let name = if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
                "webrtc.lib"
            } else {
                "libwebrtc.a"
            };
            let library = root.join("lib").join(name);
            println!("cargo:rerun-if-changed={}", library.display());
            let bytes = fs::read(&library).expect("Read the custom WebRTC library");
            let hash = format!("{:x}", Sha256::digest(bytes));
            // webrtc-sys watches LK_CUSTOM_WEBRTC, not the library's bytes.
            // A content-addressed path makes every SDK change invalidate that
            // dependency as well; an in-place change must never attest stale code.
            assert_eq!(
                root.file_name().and_then(|name| name.to_str()),
                Some(hash.as_str()),
                "Seal the SDK first: python3 seal-webrtc.py --library PATH; use its output as LK_CUSTOM_WEBRTC"
            );
            hash
        }
    };
    println!("cargo:rustc-env=WONDER_LINKED_WEBRTC_SHA256={binding}");
}
