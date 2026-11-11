//! Content-free diagnostics from paired developer builds. Never enters chat history.
use super::*;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, sync::Mutex};

pub(super) const MAX_BATCH: usize = 256 * 1024;
const MAX_STORAGE: u64 = 200 * 1024 * 1024;
const RETENTION_MS: u64 = 14 * 24 * 60 * 60 * 1000;
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Frame {
    #[serde(rename = "binaryUUID")]
    binary_uuid: uuid::Uuid,
    offset: u64,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Event {
    operation: String,
    phase: String,
    elapsed_ms: f64,
    duration_ms: f64,
    count: u64,
    bytes: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    frames: Vec<Frame>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Batch {
    version: u32,
    id: uuid::Uuid,
    session_id: uuid::Uuid,
    profile: String,
    build: String,
    app_version: String,
    device_model: String,
    os_version: String,
    events: Vec<Event>,
}
fn valid(batch: &Batch) -> bool {
    let label = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".,_- ".contains(&c))
    };
    batch.version == 1
        && batch.profile == "diagnostics"
        && label(&batch.build)
        && label(&batch.app_version)
        && label(&batch.device_model)
        && label(&batch.os_version)
        && !batch.events.is_empty()
        && batch.events.len() <= 256
        && batch.events.iter().all(|e| {
            matches!(
                e.operation.as_str(),
                "launch"
                    | "chat.open"
                    | "activity.expand"
                    | "detail.expand"
                    | "history.load"
                    | "scroll.bottom"
                    | "project.create"
                    | "project.archive"
                    | "network"
                    | "decode"
                    | "projection"
                    | "main.probe"
                    | "display.gap"
                    | "memory"
                    | "session"
                    | "capture"
                    | "scenario"
                    | "system.metric"
                    | "system.crash"
                    | "system.hang"
                    | "system.cpu"
                    | "system.disk"
            ) && matches!(
                e.phase.as_str(),
                "duration"
                    | "readiness.proxy"
                    | "start"
                    | "end"
                    | "interrupted"
                    | "failed"
                    | "sample"
            ) && e.elapsed_ms.is_finite()
                && e.elapsed_ms >= 0.
                && e.duration_ms.is_finite()
                && e.duration_ms >= 0.
                && e.metrics.len() <= 128
                && e.frames.len() <= 128
                && e.metrics.iter().all(|(k, v)| {
                    k.len() <= 160
                        && k.bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"._[]".contains(&c))
                        && v.is_finite()
                })
        })
}

pub(super) async fn ingest(
    State(state): State<AppState>,
    device: Option<Extension<AuthenticatedDevice>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(Extension(device)) = device else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if body.len() > MAX_BATCH {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let batch: Batch = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !valid(&batch) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let root = state.data_dir.join("diagnostics");
    let device = device.device_id;
    match tokio::task::spawn_blocking(move || persist(&root, &device, &batch, now_ms())).await {
        Ok(Ok(true)) => Json(serde_json::json!({"accepted":true})).into_response(),
        Ok(Ok(false)) => StatusCode::CONFLICT.into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
fn persist(root: &Path, device: &str, batch: &Batch, now: u64) -> std::io::Result<bool> {
    use std::io::Write;
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| std::io::Error::other("diagnostics lock"))?;
    crate::platform::create_private_dir(root, true)?;
    let device_key = hex::encode(Sha256::digest(device.as_bytes()));
    let path = root.join(format!("{}-{}.json", device_key, batch.id));
    let payload = serde_json::to_value(batch)?;
    if path.exists() {
        let existing: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        return Ok(existing.get("batch") == Some(&payload));
    }
    let bytes = serde_json::to_vec(
        &serde_json::json!({"receivedAtMs":now,"device":device_key,"batch":payload}),
    )?;
    prune(root, now, bytes.len() as u64, MAX_STORAGE, RETENTION_MS)?;
    let temp = path.with_extension("tmp");
    let mut file = crate::platform::create_private_file(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    Ok(true)
}
fn prune(root: &Path, now: u64, incoming: u64, max: u64, retention: u64) -> std::io::Result<()> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        let age = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if now.saturating_sub(age) > retention
            || entry.path().extension().is_some_and(|e| e == "tmp")
        {
            std::fs::remove_file(entry.path())?;
        } else {
            files.push((age, metadata.len(), entry.path()));
        }
    }
    files.sort_by_key(|f| f.0);
    let mut total = incoming + files.iter().map(|f| f.1).sum::<u64>();
    for (_, size, path) in files {
        if total <= max {
            break;
        }
        std::fs::remove_file(path)?;
        total -= size;
    }
    Ok(())
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn batch() -> Batch {
        serde_json::from_value(serde_json::json!({"version":1,"id":uuid::Uuid::new_v4(),"sessionId":uuid::Uuid::new_v4(),"profile":"diagnostics","build":"12","appVersion":"1.0","deviceModel":"iPhone15,2","osVersion":"26.6.2","events":[{"operation":"activity.expand","phase":"duration","elapsedMs":1.0,"durationMs":2.0,"count":1,"bytes":0}]})).unwrap()
    }
    #[test]
    fn retries_are_idempotent_and_conflicts_do_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = batch();
        assert!(valid(&b));
        assert!(persist(dir.path(), "device", &b, now_ms()).unwrap());
        assert!(persist(dir.path(), "device", &b, now_ms()).unwrap());
        b.events[0].count = 2;
        assert!(!persist(dir.path(), "device", &b, now_ms()).unwrap());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn rejects_content_and_rotates_to_budget() {
        let mut b = batch();
        b.events[0].operation = "secret message".into();
        assert!(!valid(&b));
        let mut value = serde_json::to_value(batch()).unwrap();
        value["prompt"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<Batch>(value).is_err());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.json"), vec![0; 20]).unwrap();
        prune(dir.path(), now_ms(), 10, 15, RETENTION_MS).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[tokio::test]
    async fn requires_paired_identity_and_valid_batch() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let body = axum::body::Bytes::from(serde_json::to_vec(&batch()).unwrap());
        assert_eq!(
            ingest(State(state.clone()), None, body).await.status(),
            StatusCode::FORBIDDEN
        );
        let device = || {
            Some(Extension(AuthenticatedDevice {
                device_id: "paired-test-device".into(),
                session_binding: "test-session".into(),
            }))
        };
        assert_eq!(
            ingest(
                State(state.clone()),
                device(),
                axum::body::Bytes::from(vec![0; MAX_BATCH + 1])
            )
            .await
            .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            ingest(
                State(state),
                device(),
                axum::body::Bytes::from_static(b"{}")
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    #[test]
    fn rejects_unknown_versions_invalid_metrics_and_excessive_events() {
        let mut b = batch();
        b.version = 2;
        assert!(!valid(&b));
        b = batch();
        b.events[0].duration_ms = f64::NAN;
        assert!(!valid(&b));
        b = batch();
        b.events[0]
            .metrics
            .insert("https://private.example/file".into(), 1.);
        assert!(!valid(&b));
        b = batch();
        b.events = (0..257).map(|_| batch().events.remove(0)).collect();
        assert!(!valid(&b));
    }
    #[test]
    fn accepts_swift_metrickit_frame_keys() {
        let mut value = serde_json::to_value(batch()).unwrap();
        value["events"][0]["frames"] =
            serde_json::json!([{"binaryUUID":uuid::Uuid::new_v4(),"offset":42}]);
        let parsed: Batch = serde_json::from_value(value).unwrap();
        assert!(valid(&parsed));
        assert_eq!(parsed.events[0].frames[0].offset, 42);
    }
    #[test]
    fn expired_reports_and_interrupted_temporary_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("interrupted.tmp"), b"{}").unwrap();
        prune(
            dir.path(),
            now_ms() + RETENTION_MS + 1000,
            0,
            MAX_STORAGE,
            RETENTION_MS,
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
