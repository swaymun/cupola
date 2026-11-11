//! Owned CLI fixtures for this computer's native sign-in status.
use crate::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
pub(crate) struct Fixture {
    pub dir: tempfile::TempDir,
    pub state: AppState,
}

impl Fixture {
    pub(crate) async fn new() -> Self {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        state.projects.codex.lock().await.shutdown().await.unwrap();
        std::fs::write(dir.path().join("status-fake.py"), "import json,sys\nfor line in sys.stdin:\n r=json.loads(line)\n if 'id' in r: print(json.dumps({'id':r['id'],'result':{}}),flush=True)\n").unwrap();
        let schemas = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../research/codex-app-server/0.155.0-alpha.9");
        // Beside the fixture's code-mode helper, which launch verification needs.
        let bin = dir.path().join("codex-status");
        std::fs::write(&bin, format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'codex-cli 0.155.0-alpha.9'; exit 0; fi\nif [ \"$1 $2\" = 'login status' ]; then echo \"$CODEX_HOME\" >> \"$(dirname \"$0\")/codex-cli-status.log\"; if [ -f \"$CODEX_HOME/fake-status-broken\" ]; then echo 'unexpected'; exit 7; fi; if [ -f \"$CODEX_HOME/fake-sign-in.json\" ]; then echo 'Logged in using ChatGPT'; exit 0; fi; echo 'Not logged in' >&2; exit 1; fi\nif [ \"$2\" = generate-json-schema ]; then last=''; for arg in \"$@\"; do last=\"$arg\"; done; flavor=stable; if [ \"$3\" = --experimental ]; then flavor=experimental; fi; cp '{}/'\"$flavor\"'/codex_app_server_protocol.v2.schemas.json' \"$last/codex_app_server_protocol.v2.schemas.json\"; exit 0; fi\nexec python3 '{}'\n",
            schemas.display(),
            dir.path().join("status-fake.py").display()
        ))
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        state.projects.set_codex_bin(bin);
        let mac = dir.path().join("mac-home");
        std::fs::create_dir_all(&mac).unwrap();
        std::fs::write(
            mac.join("fake-sign-in.json"),
            json!({"type":"chatgpt","email":"owner@example.com","planType":"pro"}).to_string(),
        )
        .unwrap();
        state.codex_home = mac.clone();
        state.projects.config.lock().unwrap().codex_home = Some(mac);
        Self { dir, state }
    }

    pub(crate) async fn shutdown(&self) {
        self.state.projects.shutdown().await;
    }
}
