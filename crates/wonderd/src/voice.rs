//! Voice mode: the owner's OpenAI key stays on this Mac. A paired phone asks
//! for a short-lived OpenAI Realtime client secret per voice session and never
//! receives the key itself. Responses and logs never carry the key, and
//! OpenAI's error bodies are not forwarded because they can quote part of it.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use wonder_store::{
    VoicePanel, VoiceSettings, VoiceTranscriptEntry, VoiceUsageEvent,
    VOICE_SESSION_LIMIT_MAX_MINUTES,
};

use crate::{AppState, AuthenticatedDevice, OwnerAuthority};

pub(crate) const FEATURE: &str = "voice.openai.v1";
/// Mints need a panel ID; limits, usage reports and transcripts are here.
pub(crate) const LIMITS_FEATURE: &str = "voice.limits.v1";
const OPENAI_API: &str = "https://api.openai.com";
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.swaymun.wonder.voice.openai";
/// The realtime model voice sessions use. `WONDER_VOICE_MODEL` overrides it
/// for development; the phone learns the model from each minted session.
const DEFAULT_MODEL: &str = "gpt-realtime-2.1";
const TRANSCRIPTION_MODEL: &str = "gpt-4o-mini-transcribe";
const VOICE: &str = "marin";
/// How long a minted secret can open a connection. The phone connects at once.
const CLIENT_SECRET_SECONDS: u64 = 120;
const MAX_KEY_BYTES: usize = 512;
/// Client secrets one open voice panel may mint: the first connection and a
/// few reconnects after a dropped socket.
const PANEL_MINTS_MAX: u32 = 6;
/// Client secrets one device may mint per `DEVICE_MINT_WINDOW`.
const DEVICE_MINTS_MAX: usize = 6;
const DEVICE_MINT_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Panels without a saved transcript are forgotten after this long.
const PANEL_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The Mac itself (loopback owner), which has no paired device ID.
const THIS_MAC: &str = "this-mac";

/// Where the OpenAI key is kept. The daemon uses the login Keychain; tests use
/// memory.
pub trait VoiceKeyStore: Send + Sync {
    fn read(&self) -> Result<Option<String>, String>;
    fn write(&self, key: &str) -> Result<(), String>;
    fn remove(&self) -> Result<(), String>;
}

/// A generic-password item in the login Keychain, one per Wonder data home so
/// a development daemon never reads or replaces the installed app's key.
/// Elsewhere (Linux), a file only this user can read inside the data home.
pub struct KeychainKeyStore {
    #[cfg(target_os = "macos")]
    account: String,
    #[cfg(all(unix, not(target_os = "macos")))]
    path: std::path::PathBuf,
}

impl KeychainKeyStore {
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub fn for_data_home(data_home: &std::path::Path) -> Self {
        Self {
            #[cfg(target_os = "macos")]
            account: crate::data_home::keychain_account(data_home),
            #[cfg(all(unix, not(target_os = "macos")))]
            path: data_home.join("secrets").join("voice-openai-key"),
        }
    }
}

#[cfg(target_os = "macos")]
impl VoiceKeyStore for KeychainKeyStore {
    fn read(&self) -> Result<Option<String>, String> {
        use security_framework::passwords::get_generic_password;
        match get_generic_password(KEYCHAIN_SERVICE, &self.account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| "The saved OpenAI key is unreadable. Add it again.".into()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(_) => Err("The Mac Keychain could not be read.".into()),
        }
    }

    fn write(&self, key: &str) -> Result<(), String> {
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            &self.account,
            key.as_bytes(),
        )
        .map_err(|_| "The Mac Keychain could not save the key.".into())
    }

    fn remove(&self) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(
            KEYCHAIN_SERVICE,
            &self.account,
        ) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            Err(_) => Err("The saved OpenAI key could not be removed.".into()),
        }
    }
}

#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// The key file is created 0600 in a 0700 folder and replaced atomically, so
/// a crash leaves the old key or the new one, never a partial or readable one.
#[cfg(all(unix, not(target_os = "macos")))]
impl VoiceKeyStore for KeychainKeyStore {
    fn read(&self) -> Result<Option<String>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(None),
            Ok(text) => Ok(Some(text.trim().to_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err("The saved OpenAI key could not be read.".into()),
        }
    }

    fn write(&self, key: &str) -> Result<(), String> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let failed = |_| "The OpenAI key could not be saved.".to_owned();
        let dir = self.path.parent().ok_or("The key folder is missing.")?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(failed)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(failed)?;
        let tmp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(failed)?;
        file.write_all(key.as_bytes()).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        std::fs::rename(&tmp, &self.path).map_err(failed)
    }

    fn remove(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("The saved OpenAI key could not be removed.".into()),
        }
    }
}

#[cfg(not(unix))]
impl VoiceKeyStore for KeychainKeyStore {
    fn read(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn write(&self, _key: &str) -> Result<(), String> {
        Err("Voice keys cannot be stored on this computer yet.".into())
    }
    fn remove(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryKeyStore(std::sync::Mutex<Option<String>>);

impl VoiceKeyStore for MemoryKeyStore {
    fn read(&self) -> Result<Option<String>, String> {
        Ok(self.0.lock().map_err(|_| "poisoned")?.clone())
    }
    fn write(&self, key: &str) -> Result<(), String> {
        *self.0.lock().map_err(|_| "poisoned")? = Some(key.to_owned());
        Ok(())
    }
    fn remove(&self) -> Result<(), String> {
        *self.0.lock().map_err(|_| "poisoned")? = None;
        Ok(())
    }
}

pub struct Voice {
    keys: Arc<dyn VoiceKeyStore>,
    api_base: String,
    model: String,
    client: reqwest::Client,
    /// Serializes key changes so a save and a removal cannot interleave.
    change: tokio::sync::Mutex<()>,
    /// Serializes mint decisions (panel, rate and budget checks) and holds
    /// each device's recent mint times.
    mints: tokio::sync::Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Voice {
    pub fn new(keys: Arc<dyn VoiceKeyStore>) -> Self {
        let model = std::env::var("WONDER_VOICE_MODEL")
            .ok()
            .filter(|model| valid_model(model))
            .unwrap_or_else(|| DEFAULT_MODEL.into());
        Self::with_api_base(keys, OPENAI_API.into(), model)
    }

    /// The installed daemon: the login Keychain, one item per data home.
    pub fn keychain(data_home: &std::path::Path) -> Self {
        Self::new(Arc::new(KeychainKeyStore::for_data_home(data_home)))
    }

    pub fn in_memory() -> Self {
        Self::new(Arc::new(MemoryKeyStore::default()))
    }

    fn with_api_base(keys: Arc<dyn VoiceKeyStore>, api_base: String, model: String) -> Self {
        Self {
            keys,
            api_base,
            model,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("voice HTTP client configuration"),
            change: tokio::sync::Mutex::new(()),
            mints: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn read_key(&self) -> Result<Option<String>, String> {
        let keys = Arc::clone(&self.keys);
        tokio::task::spawn_blocking(move || keys.read())
            .await
            .map_err(|_| "The saved OpenAI key could not be read.".to_owned())?
    }

    async fn status(&self) -> Result<Value, String> {
        let configured = self.read_key().await?.is_some();
        Ok(json!({
            "provider": "openai",
            "configured": configured,
            "model": self.model,
        }))
    }
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 64
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// A plausible OpenAI secret key: `sk-` and printable, with no whitespace.
fn plausible_key(key: &str) -> bool {
    key.starts_with("sk-")
        && (20..=MAX_KEY_BYTES).contains(&key.len())
        && key.chars().all(|c| c.is_ascii_graphic())
}

fn failure(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({ "detail": detail }))).into_response()
}

/// `GET /api/v1/voice`: whether voice mode has a key on this Mac.
pub(crate) async fn status(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
) -> Response {
    match state.voice.status().await {
        Ok(status) => Json(status).into_response(),
        Err(detail) => failure(StatusCode::SERVICE_UNAVAILABLE, &detail),
    }
}

/// How long a credit reading (or a refusal) is reused. The phone refreshes
/// Usage every minute; OpenAI's legacy billing route is slow and may refuse
/// ordinary secret keys, so ask it rarely.
const CREDITS_TTL: std::time::Duration = std::time::Duration::from_secs(600);

static CREDITS_CACHE: std::sync::Mutex<Option<(String, std::time::Instant, Value)>> =
    std::sync::Mutex::new(None);

/// `GET /api/v1/voice/credits`: the prepaid credit left on the saved OpenAI key,
/// when OpenAI's billing API lets this key read it. Never repeats the key.
/// Answers `{"available": false}` when it can't (OpenAI documents no
/// supported credit-balance API for ordinary keys).
pub(crate) async fn credits(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
) -> Response {
    use sha2::{Digest, Sha256};
    let voice = &state.voice;
    let Ok(Some(key)) = voice.read_key().await else {
        return Json(json!({ "available": false })).into_response();
    };
    // Cached under a digest of the key so a changed key is read again at once.
    let fingerprint = hex::encode(&Sha256::digest(key.as_bytes())[..8]);
    if let Ok(cache) = CREDITS_CACHE.lock() {
        if let Some((cached_for, at, body)) = cache.as_ref() {
            if *cached_for == fingerprint && at.elapsed() < CREDITS_TTL {
                return Json(body.clone()).into_response();
            }
        }
    }
    let body = read_credits(voice, &key).await;
    if let Ok(mut cache) = CREDITS_CACHE.lock() {
        *cache = Some((fingerprint, std::time::Instant::now(), body.clone()));
    }
    Json(body).into_response()
}

async fn read_credits(voice: &Voice, key: &str) -> Value {
    let unavailable = || json!({ "available": false });
    let read = voice
        .client
        .get(format!(
            "{}/v1/dashboard/billing/credit_grants",
            voice.api_base
        ))
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await;
    let Ok(response) = read else {
        return unavailable();
    };
    if !response.status().is_success() {
        return unavailable();
    }
    let Ok(body) = response.json::<Value>().await else {
        return unavailable();
    };
    match body["total_available"].as_f64() {
        Some(left) if left.is_finite() && left >= 0.0 => json!({
            "available": true,
            "remainingUsd": left,
            "grantedUsd": body["total_granted"].as_f64(),
        }),
        _ => unavailable(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SaveKey {
    api_key: String,
}

/// `PUT /api/v1/voice/openai-key`: checks the key with OpenAI, then keeps it
/// in this Mac's Keychain. The response never repeats it.
pub(crate) async fn save_key(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    Json(request): Json<SaveKey>,
) -> Response {
    let key = request.api_key.trim().to_owned();
    if !plausible_key(&key) {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "That is not an OpenAI API key. Keys start with sk-.",
        );
    }
    let voice = &state.voice;
    let _change = voice.change.lock().await;
    let check = voice
        .client
        .get(format!("{}/v1/models/{}", voice.api_base, voice.model))
        .bearer_auth(&key)
        .send()
        .await;
    let status = match check {
        Ok(response) => response.status(),
        Err(_) => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "OpenAI could not be reached to check the key. Try again.",
            )
        }
    };
    if status == StatusCode::UNAUTHORIZED {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "OpenAI did not accept this key. Check it and try again.",
        );
    }
    if status == StatusCode::NOT_FOUND || status == StatusCode::FORBIDDEN {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "This key cannot use OpenAI's realtime voice model. Check the key's project permissions.",
        );
    }
    if !status.is_success() {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI could not check the key right now. Try again.",
        );
    }
    let keys = Arc::clone(&voice.keys);
    let stored = tokio::task::spawn_blocking(move || keys.write(&key))
        .await
        .unwrap_or_else(|_| Err("The OpenAI key could not be saved.".into()));
    if let Err(detail) = stored {
        return failure(StatusCode::SERVICE_UNAVAILABLE, &detail);
    }
    let _ = state
        .logger
        .record("info", "voice.key_saved", json!({ "provider": "openai" }));
    match voice.status().await {
        Ok(status) => Json(status).into_response(),
        Err(detail) => failure(StatusCode::SERVICE_UNAVAILABLE, &detail),
    }
}

/// `DELETE /api/v1/voice/openai-key`: removes the key from this Mac.
pub(crate) async fn remove_key(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
) -> Response {
    let _change = state.voice.change.lock().await;
    let keys = Arc::clone(&state.voice.keys);
    let removed = tokio::task::spawn_blocking(move || keys.remove())
        .await
        .unwrap_or_else(|_| Err("The saved OpenAI key could not be removed.".into()));
    if let Err(detail) = removed {
        return failure(StatusCode::SERVICE_UNAVAILABLE, &detail);
    }
    let _ = state
        .logger
        .record("info", "voice.key_removed", json!({ "provider": "openai" }));
    match state.voice.status().await {
        Ok(status) => Json(status).into_response(),
        Err(detail) => failure(StatusCode::SERVICE_UNAVAILABLE, &detail),
    }
}

/// Most dictionary words sent to the transcriber; more dilutes the prompt.
const DICTIONARY_PROMPT_WORDS: usize = 100;
/// Longest single dictionary entry, in characters.
const DICTIONARY_WORD_CHARS: usize = 40;

/// The transcriber's prompt from the owner's dictionary, `None` when empty.
fn transcription_prompt(words: &[String]) -> Option<String> {
    if words.is_empty() {
        return None;
    }
    let words: Vec<&str> = words
        .iter()
        .take(DICTIONARY_PROMPT_WORDS)
        .map(String::as_str)
        .collect();
    Some(format!(
        "The speaker may use these words and names: {}.",
        words.join(", ")
    ))
}

/// One dictionary entry as the owner typed it: single line, trimmed, bounded.
fn clean_word(word: &str) -> Option<String> {
    let word = word.split_whitespace().collect::<Vec<_>>().join(" ");
    (!word.is_empty() && word.chars().count() <= DICTIONARY_WORD_CHARS && !word.contains(','))
        .then_some(word)
}

/// The session the Mac asks OpenAI for. Instructions and tools come from the
/// phone after it connects, because the phone carries out the tool calls.
fn session_request(model: &str, words: &[String]) -> Value {
    let mut transcription = json!({ "model": TRANSCRIPTION_MODEL });
    if let Some(prompt) = transcription_prompt(words) {
        transcription["prompt"] = prompt.into();
    }
    json!({
        "expires_after": { "anchor": "created_at", "seconds": CLIENT_SECRET_SECONDS },
        "session": {
            "type": "realtime",
            "model": model,
            "output_modalities": ["audio"],
            "audio": {
                "input": {
                    "format": { "type": "audio/pcm", "rate": 24000 },
                    "noise_reduction": { "type": "near_field" },
                    "transcription": transcription,
                    "turn_detection": {
                        "type": "semantic_vad",
                        // Short app commands should finish promptly, while
                        // semantic VAD still permits pauses within an utterance.
                        "eagerness": "high",
                        "create_response": true,
                        "interrupt_response": true
                    }
                },
                "output": {
                    "format": { "type": "audio/pcm", "rate": 24000 },
                    "voice": VOICE
                }
            }
        }
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MintRequest {
    panel_id: String,
}

/// A panel ID the phone generates each time it opens voice.
fn valid_panel_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn device_id(device: &Option<Extension<AuthenticatedDevice>>) -> String {
    device
        .as_ref()
        .map_or_else(|| THIS_MAC.to_owned(), |device| device.device_id.clone())
}

/// The start of today on this Mac's clock, in Unix milliseconds.
fn day_start_ms(now_ms: i64) -> i64 {
    use chrono::{Local, TimeZone};
    let Some(now) = Local.timestamp_millis_opt(now_ms).single() else {
        return now_ms - now_ms.rem_euclid(86_400_000);
    };
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
        .map_or(now_ms - now_ms.rem_euclid(86_400_000), |start| {
            start.timestamp_millis()
        })
}

/// Seconds a panel counts against the daily minutes. The phone's reported
/// connected time when it reported anything; otherwise the time the panel
/// has been open on the Mac, up to its session limit, so a phone that never
/// reports still uses up the budget.
fn billed_seconds(panel: &VoicePanel, now_ms: i64) -> i64 {
    if panel.connected_seconds > 0 || panel.usage_events > 0 {
        return i64::from(panel.connected_seconds);
    }
    let until = panel.ended_at_ms.unwrap_or(now_ms).min(panel.ends_at_ms);
    ((until - panel.opened_at_ms) / 1000).max(0)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Today {
    seconds: i64,
    cost_microdollars: i64,
}

impl Today {
    fn json(self) -> Value {
        json!({
            "spokenSeconds": self.seconds,
            "estimatedDollars": cents((self.cost_microdollars.max(0) + 9_999) / 10_000),
        })
    }
}

fn cents(cents: i64) -> f64 {
    cents as f64 / 100.0
}

async fn today(state: &AppState, now_ms: i64) -> Result<Today, sqlx::Error> {
    let panels = state.store.voice_panels_since(day_start_ms(now_ms)).await?;
    Ok(panels.iter().fold(Today::default(), |today, panel| Today {
        seconds: today.seconds + billed_seconds(panel, now_ms),
        cost_microdollars: today.cost_microdollars + panel.cost_microdollars,
    }))
}

/// Why a mint is refused, with the reason the phone shows.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Refusal {
    InvalidRequest,
    OtherDevice,
    PanelEnded,
    SessionLimit(u32),
    PanelMints,
    RateLimited,
    DailyMinutes(u32),
    DailyCost(u32),
}

impl Refusal {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::OtherDevice => "other_device",
            Self::PanelEnded => "panel_ended",
            Self::SessionLimit(_) => "session_limit",
            Self::PanelMints => "panel_mints",
            Self::RateLimited => "rate_limited",
            Self::DailyMinutes(_) => "daily_minutes",
            Self::DailyCost(_) => "daily_cost",
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::InvalidRequest => "Update Cupola on this phone to use voice.".into(),
            Self::OtherDevice => "This voice session belongs to another device. Start voice again.".into(),
            Self::PanelEnded => "This voice session has ended. Start voice again.".into(),
            Self::SessionLimit(minutes) => format!(
                "This voice session reached its {minutes}-minute limit. Start voice again to keep talking."
            ),
            Self::PanelMints => "Voice reconnected too many times. Close voice and start it again.".into(),
            Self::RateLimited => "Voice was started too many times in the last few minutes. Wait a few minutes and try again.".into(),
            Self::DailyMinutes(minutes) => format!(
                "Today's voice limit of {minutes} minute{} is used up. It resets at midnight, or you can raise it in Voice settings.",
                if *minutes == 1 { "" } else { "s" }
            ),
            Self::DailyCost(limit) => format!(
                "Today's voice spending limit of ${:.2} is used up. It resets at midnight, or you can raise it in Voice settings.",
                cents(i64::from(*limit))
            ),
        }
    }
}

/// Whether this mint may go ahead. `recent` is the device's mint times in
/// the rate window, already pruned.
fn check_mint(
    panel: Option<&VoicePanel>,
    device: &str,
    settings: &VoiceSettings,
    today: Today,
    recent: usize,
    now_ms: i64,
) -> Result<(), Refusal> {
    if let Some(panel) = panel {
        if panel.device_id != device {
            return Err(Refusal::OtherDevice);
        }
        if panel.ended_at_ms.is_some() {
            return Err(Refusal::PanelEnded);
        }
        if now_ms >= panel.ends_at_ms {
            let minutes = (panel.ends_at_ms - panel.opened_at_ms) / 60_000;
            return Err(Refusal::SessionLimit(minutes.max(1) as u32));
        }
        if panel.mints >= PANEL_MINTS_MAX {
            return Err(Refusal::PanelMints);
        }
    }
    if recent >= DEVICE_MINTS_MAX {
        return Err(Refusal::RateLimited);
    }
    if let Some(minutes) = settings.daily_minutes {
        if today.seconds >= i64::from(minutes) * 60 {
            return Err(Refusal::DailyMinutes(minutes));
        }
    }
    if let Some(limit) = settings.daily_cents {
        if today.cost_microdollars >= i64::from(limit) * 10_000 {
            return Err(Refusal::DailyCost(limit));
        }
    }
    Ok(())
}

fn refuse(state: &AppState, refusal: Refusal, device: &str) -> Response {
    let _ = state.logger.record(
        "warning",
        "voice.session_refused",
        json!({ "provider": "openai", "reason": refusal.code(), "device": device }),
    );
    failure(StatusCode::CONFLICT, &refusal.detail())
}

fn store_failure() -> Response {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "Voice settings on your computer could not be read. Try again.",
    )
}

/// `POST /api/v1/voice/sessions`: a short-lived Realtime client secret for one
/// voice session on a paired device. Minted only for an open voice panel
/// (`panelId`, ended with `DELETE /api/v1/voice/panels/{panelId}`), within
/// the panel's session limit, the device's mint rate and today's budget.
pub(crate) async fn create_session(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    device: Option<Extension<AuthenticatedDevice>>,
    body: Bytes,
) -> Response {
    let device = device_id(&device);
    let Some(request) = serde_json::from_slice::<MintRequest>(&body)
        .ok()
        .filter(|request| valid_panel_id(&request.panel_id))
    else {
        return refuse(&state, Refusal::InvalidRequest, &device);
    };
    let voice = &state.voice;
    let mut mints = voice.mints.lock().await;
    let now = Instant::now();
    let now_ms = crate::now_ms() as i64;
    let recent = mints.entry(device.clone()).or_default();
    while recent
        .front()
        .is_some_and(|at| now.duration_since(*at) >= DEVICE_MINT_WINDOW)
    {
        recent.pop_front();
    }
    let (settings, panel, used) = match (
        state.store.voice_settings().await,
        state.store.voice_panel(&request.panel_id).await,
        today(&state, now_ms).await,
    ) {
        (Ok(settings), Ok(panel), Ok(used)) => (settings, panel, used),
        _ => return store_failure(),
    };
    if let Err(refusal) = check_mint(
        panel.as_ref(),
        &device,
        &settings,
        used,
        recent.len(),
        now_ms,
    ) {
        return refuse(&state, refusal, &device);
    }
    let key = match voice.read_key().await {
        Ok(Some(key)) => key,
        Ok(None) => {
            return failure(
                StatusCode::CONFLICT,
                "Add an OpenAI API key to use voice. Add it in Settings on this phone or with cupola config set voice-key on your computer.",
            )
        }
        Err(detail) => return failure(StatusCode::SERVICE_UNAVAILABLE, &detail),
    };
    // Read at session start only; later edits apply to the next session.
    let words = state.store.voice_words().await.unwrap_or_default();
    // Every request that reaches OpenAI counts toward the rate, also failed
    // ones.
    recent.push_back(now);
    let response = voice
        .client
        .post(format!("{}/v1/realtime/client_secrets", voice.api_base))
        .bearer_auth(&key)
        .json(&session_request(&voice.model, &words))
        .send()
        .await;
    drop(key);
    let response = match response {
        Ok(response) => response,
        Err(_) => {
            return failure(
                StatusCode::BAD_GATEWAY,
                "OpenAI could not be reached from your computer. Try again.",
            )
        }
    };
    let status = response.status();
    let _ = state.logger.record(
        if status.is_success() {
            "info"
        } else {
            "warning"
        },
        "voice.session_minted",
        json!({
            "provider": "openai",
            "model": voice.model,
            "status": status.as_u16(),
            "device": device,
            "panel": request.panel_id,
            "mint": panel.as_ref().map_or(1, |panel| panel.mints + 1),
        }),
    );
    if status == StatusCode::UNAUTHORIZED {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI no longer accepts the saved key. Replace it in Settings.",
        );
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI's rate limit or quota for this key was reached. Check your OpenAI billing, then try again.",
        );
    }
    if !status.is_success() {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI could not start a voice session. Try again.",
        );
    }
    let Ok(body) = response.json::<Value>().await else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI sent an unexpected reply. Try again.",
        );
    };
    let (Some(secret), Some(expires_at)) = (
        body.get("value").and_then(Value::as_str),
        body.get("expires_at").and_then(Value::as_i64),
    ) else {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI sent an unexpected reply. Try again.",
        );
    };
    // Only an ephemeral secret goes to the phone, never something shaped
    // like a long-lived key.
    if !secret.starts_with("ek_") {
        return failure(
            StatusCode::BAD_GATEWAY,
            "OpenAI sent an unexpected reply. Try again.",
        );
    }
    let limit_ms = i64::from(settings.session_limit_minutes) * 60_000;
    let panel_ends_at_ms = panel
        .as_ref()
        .map_or(now_ms + limit_ms, |panel| panel.ends_at_ms);
    if state
        .store
        .record_voice_mint(&request.panel_id, &device, now_ms, panel_ends_at_ms)
        .await
        .is_err()
    {
        return store_failure();
    }
    let _ = state
        .store
        .prune_voice_panels(now_ms - PANEL_RETENTION_MS)
        .await;
    // The phone ends the session cleanly at the panel's limit, or earlier
    // when today's minutes run out first.
    let (ends_at_ms, ends_for) = match settings.daily_minutes {
        Some(minutes)
            if now_ms + (i64::from(minutes) * 60 - used.seconds) * 1000 < panel_ends_at_ms =>
        {
            (
                now_ms + (i64::from(minutes) * 60 - used.seconds) * 1000,
                "dailyLimit",
            )
        }
        _ => (panel_ends_at_ms, "sessionLimit"),
    };
    Json(json!({
        "provider": "openai",
        "clientSecret": secret,
        "expiresAt": expires_at,
        "model": voice.model,
        "url": format!("wss://api.openai.com/v1/realtime?model={}", voice.model),
        "panelId": request.panel_id,
        "sessionLimitSeconds": limit_ms / 1000,
        "sessionEndsAt": ends_at_ms / 1000,
        "sessionEndsFor": ends_for,
    }))
    .into_response()
}

/// The panel this device opened, or the reason the phone cannot report to it.
async fn owned_panel(
    state: &AppState,
    panel_id: &str,
    device: &str,
) -> Result<VoicePanel, Response> {
    match state.store.voice_panel(panel_id).await {
        Ok(Some(panel)) if panel.device_id == device => Ok(panel),
        Ok(_) => Err(failure(
            StatusCode::NOT_FOUND,
            "This voice session is not on your computer.",
        )),
        Err(_) => Err(store_failure()),
    }
}

/// `DELETE /api/v1/voice/panels/{panelId}`: the voice panel closed, so no
/// more secrets are minted for it.
pub(crate) async fn end_panel(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    device: Option<Extension<AuthenticatedDevice>>,
    Path(panel_id): Path<String>,
) -> Response {
    let device = device_id(&device);
    if let Err(response) = owned_panel(&state, &panel_id, &device).await {
        return response;
    }
    match state
        .store
        .end_voice_panel(&panel_id, crate::now_ms() as i64)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => store_failure(),
    }
}

const USAGE_EVENTS_MAX: usize = 200;
const ID_MAX: usize = 128;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UsageReport {
    /// The phone's running total of connected seconds for the panel.
    connected_seconds: u32,
    #[serde(default)]
    events: Vec<UsageEventReport>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UsageEventReport {
    source: String,
    id: String,
    /// OpenAI's `usage` object from `response.done` or a transcription event.
    usage: Value,
}

/// OpenAI's published USD rates per 1M tokens (2026-10-18; the same table
/// the Diagnostics harness uses), i.e. microdollars per token. An estimate,
/// not an invoice.
mod rate {
    pub const TEXT_IN: f64 = 4.0;
    pub const AUDIO_IN: f64 = 32.0;
    pub const IMAGE_IN: f64 = 5.0;
    pub const TEXT_CACHED: f64 = 0.4;
    pub const AUDIO_CACHED: f64 = 0.4;
    pub const IMAGE_CACHED: f64 = 0.5;
    pub const TEXT_OUT: f64 = 24.0;
    pub const AUDIO_OUT: f64 = 64.0;
    pub const TRANSCRIBE_IN: f64 = 1.25;
    pub const TRANSCRIBE_OUT: f64 = 5.0;
    /// $0.003 per minute.
    pub const TRANSCRIBE_PER_SECOND: f64 = 50.0;
}

/// Estimated cost of one usage event in microdollars, or `None` when it is
/// not a usage event voice knows.
fn usage_cost(source: &str, usage: &Value) -> Option<i64> {
    let count = |value: &Value, field: &str| {
        value
            .get(field)
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .max(0.0)
    };
    let cost = match source {
        "transcription" if usage.get("type").and_then(Value::as_str) == Some("duration") => {
            count(usage, "seconds") * rate::TRANSCRIBE_PER_SECOND
        }
        "transcription" => {
            count(usage, "input_tokens") * rate::TRANSCRIBE_IN
                + count(usage, "output_tokens") * rate::TRANSCRIBE_OUT
        }
        "response" => {
            let inputs = usage.get("input_token_details").unwrap_or(&Value::Null);
            let outputs = usage.get("output_token_details").unwrap_or(&Value::Null);
            let cached = inputs.get("cached_tokens_details").unwrap_or(&Value::Null);
            let mut cost = 0.0;
            for (field, full, cache) in [
                ("text_tokens", rate::TEXT_IN, rate::TEXT_CACHED),
                ("audio_tokens", rate::AUDIO_IN, rate::AUDIO_CACHED),
                ("image_tokens", rate::IMAGE_IN, rate::IMAGE_CACHED),
            ] {
                let hits = count(cached, field);
                cost += (count(inputs, field) - hits).max(0.0) * full + hits * cache;
            }
            cost + count(outputs, "text_tokens") * rate::TEXT_OUT
                + count(outputs, "audio_tokens") * rate::AUDIO_OUT
        }
        _ => return None,
    };
    Some(cost.round() as i64)
}

/// `POST /api/v1/voice/panels/{panelId}/usage`: the usage OpenAI reported to
/// the phone during the session. Answers with today's totals.
pub(crate) async fn report_usage(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    device: Option<Extension<AuthenticatedDevice>>,
    Path(panel_id): Path<String>,
    Json(report): Json<UsageReport>,
) -> Response {
    let device = device_id(&device);
    let panel = match owned_panel(&state, &panel_id, &device).await {
        Ok(panel) => panel,
        Err(response) => return response,
    };
    if report.events.len() > USAGE_EVENTS_MAX {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Too many usage events in one report.",
        );
    }
    let mut events = Vec::with_capacity(report.events.len());
    for event in report.events {
        let cost = usage_cost(&event.source, &event.usage);
        let (Some(cost), false) = (cost, event.id.is_empty() || event.id.len() > ID_MAX) else {
            return failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "A usage event was not understood.",
            );
        };
        events.push(VoiceUsageEvent {
            source: event.source,
            id: event.id,
            cost_microdollars: cost,
        });
    }
    // The phone cannot have been connected longer than the panel's limit,
    // plus a minute for clock and reconnect slack.
    let most = ((panel.ends_at_ms - panel.opened_at_ms) / 1000 + 60).max(0) as u32;
    let seconds = report.connected_seconds.min(most);
    if state
        .store
        .record_voice_usage(&panel_id, seconds, &events)
        .await
        .is_err()
    {
        return store_failure();
    }
    match today(&state, crate::now_ms() as i64).await {
        Ok(today) => Json(json!({ "today": today.json() })).into_response(),
        Err(_) => store_failure(),
    }
}

const TRANSCRIPT_ENTRIES_MAX: usize = 200;
const TRANSCRIPT_TEXT_MAX: usize = 8_000;
const TRANSCRIPT_THREADS_MAX: usize = 50;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TranscriptReport {
    #[serde(default)]
    entries: Vec<TranscriptEntryReport>,
    /// The threads the session acted on so far.
    #[serde(default)]
    thread_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TranscriptEntryReport {
    id: String,
    speaker: String,
    text: String,
    at_ms: i64,
}

fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_MAX && !id.chars().any(char::is_control)
}

/// `POST /api/v1/voice/panels/{panelId}/transcript`: what was said in the
/// session (text only), kept on this Mac when the owner keeps transcripts.
pub(crate) async fn save_transcript(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    device: Option<Extension<AuthenticatedDevice>>,
    Path(panel_id): Path<String>,
    Json(report): Json<TranscriptReport>,
) -> Response {
    let device = device_id(&device);
    if let Err(response) = owned_panel(&state, &panel_id, &device).await {
        return response;
    }
    let valid = report.entries.len() <= TRANSCRIPT_ENTRIES_MAX
        && report.thread_ids.len() <= TRANSCRIPT_THREADS_MAX
        && report.entries.iter().all(|entry| {
            plain_id(&entry.id)
                && matches!(entry.speaker.as_str(), "owner" | "voice")
                && entry.text.chars().count() <= TRANSCRIPT_TEXT_MAX
                && entry.at_ms >= 0
        })
        && report.thread_ids.iter().all(|id| plain_id(id));
    if !valid {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The voice transcript was not understood.",
        );
    }
    let keep = match state.store.voice_settings().await {
        Ok(settings) => settings.keep_transcripts,
        Err(_) => return store_failure(),
    };
    if keep {
        let entries: Vec<_> = report
            .entries
            .into_iter()
            .filter(|entry| !entry.text.trim().is_empty())
            .map(|entry| VoiceTranscriptEntry {
                item_id: entry.id,
                speaker: entry.speaker,
                text: entry.text.trim().to_owned(),
                at_ms: entry.at_ms,
            })
            .collect();
        if state
            .store
            .save_voice_transcript(&panel_id, &entries, &report.thread_ids)
            .await
            .is_err()
        {
            return store_failure();
        }
    }
    Json(json!({ "saved": keep })).into_response()
}

fn settings_json(settings: &VoiceSettings, today: Today) -> Value {
    json!({
        "sessionLimitMinutes": settings.session_limit_minutes,
        "sessionLimitMaxMinutes": VOICE_SESSION_LIMIT_MAX_MINUTES,
        "dailyMinutes": settings.daily_minutes,
        "dailyDollars": settings.daily_cents.map(|limit| cents(i64::from(limit))),
        "keepTranscripts": settings.keep_transcripts,
        "today": today.json(),
    })
}

async fn current_settings(state: &AppState) -> Response {
    match (
        state.store.voice_settings().await,
        today(state, crate::now_ms() as i64).await,
    ) {
        (Ok(settings), Ok(today)) => Json(settings_json(&settings, today)).into_response(),
        _ => store_failure(),
    }
}

/// `GET /api/v1/voice/settings`: the owner's voice limits, whether
/// transcripts are kept, and today's use.
pub(crate) async fn settings(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
) -> Response {
    current_settings(&state).await
}

/// A field that may be absent (unchanged) or `null` (no limit).
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SettingsChange {
    session_limit_minutes: Option<u32>,
    #[serde(default, deserialize_with = "nullable")]
    daily_minutes: Option<Option<u32>>,
    #[serde(default, deserialize_with = "nullable")]
    daily_dollars: Option<Option<f64>>,
    keep_transcripts: Option<bool>,
}

async fn current_words(state: &AppState) -> Response {
    match state.store.voice_words().await {
        Ok(words) => Json(json!({ "words": words })).into_response(),
        Err(_) => store_failure(),
    }
}

/// `GET /api/v1/voice/dictionary`: the owner's words for voice transcription.
pub(crate) async fn dictionary(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
) -> Response {
    current_words(&state).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DictionaryChange {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

/// `PUT /api/v1/voice/dictionary`: adds and removes words, then returns the
/// whole dictionary.
pub(crate) async fn update_dictionary(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    Json(change): Json<DictionaryChange>,
) -> Response {
    let mut add = Vec::new();
    for word in &change.add {
        match clean_word(word) {
            Some(word) => add.push(word),
            None => {
                return failure(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &format!(
                        "\"{word}\" cannot be a dictionary word. Use up to {DICTIONARY_WORD_CHARS} characters with no commas."
                    ),
                )
            }
        }
    }
    let Ok(existing) = state.store.voice_words().await else {
        return store_failure();
    };
    let fresh = add
        .iter()
        .filter(|word| !existing.iter().any(|have| have.eq_ignore_ascii_case(word)))
        .count();
    if existing.len() + fresh > DICTIONARY_PROMPT_WORDS {
        return failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!(
                "The dictionary holds up to {DICTIONARY_PROMPT_WORDS} words. Remove some first."
            ),
        );
    }
    let now = crate::now_ms();
    let saved = async {
        state.store.remove_voice_words(&change.remove).await?;
        state.store.add_voice_words(&add, now as i64).await
    }
    .await;
    if saved.is_err() {
        return store_failure();
    }
    current_words(&state).await
}

/// `PUT /api/v1/voice/settings`: changes the fields it names.
pub(crate) async fn update_settings(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    Json(change): Json<SettingsChange>,
) -> Response {
    let Ok(mut settings) = state.store.voice_settings().await else {
        return store_failure();
    };
    if let Some(minutes) = change.session_limit_minutes {
        if !(1..=VOICE_SESSION_LIMIT_MAX_MINUTES).contains(&minutes) {
            return failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!(
                    "Choose a session limit from 1 to {VOICE_SESSION_LIMIT_MAX_MINUTES} minutes."
                ),
            );
        }
        settings.session_limit_minutes = minutes;
    }
    if let Some(minutes) = change.daily_minutes {
        if minutes.is_some_and(|minutes| !(1..=1440).contains(&minutes)) {
            return failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose a daily limit from 1 to 1,440 minutes, or no limit.",
            );
        }
        settings.daily_minutes = minutes;
    }
    if let Some(dollars) = change.daily_dollars {
        let limit = dollars.map(|dollars| (dollars * 100.0).round());
        if limit.is_some_and(|cents| !(1.0..=100_000.0).contains(&cents)) {
            return failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Choose a daily spending limit from $0.01 to $1,000, or no limit.",
            );
        }
        settings.daily_cents = limit.map(|cents| cents as u32);
    }
    if let Some(keep) = change.keep_transcripts {
        settings.keep_transcripts = keep;
    }
    if state
        .store
        .set_voice_settings(&settings, &crate::now_ms().to_string())
        .await
        .is_err()
    {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Voice settings could not be saved on your computer. Try again.",
        );
    }
    current_settings(&state).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TranscriptQuery {
    thread_id: Option<String>,
}

/// `GET /api/v1/voice/transcripts`: saved voice sessions, newest first,
/// optionally only those that acted on one thread.
pub(crate) async fn transcripts(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    Query(query): Query<TranscriptQuery>,
) -> Response {
    match state
        .store
        .voice_transcripts(query.thread_id.as_deref(), 100)
        .await
    {
        Ok(sessions) => Json(json!({
            "sessions": sessions.iter().map(|session| json!({
                "id": session.panel_id,
                "startedAtMs": session.started_at_ms,
                "endedAtMs": session.ended_at_ms,
                "entryCount": session.entry_count,
                "preview": session.preview.chars().take(200).collect::<String>(),
                "threadIds": session.thread_ids,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(_) => store_failure(),
    }
}

/// `GET /api/v1/voice/transcripts/{panelId}`: one saved session in order.
pub(crate) async fn transcript(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
    Path(panel_id): Path<String>,
) -> Response {
    match state.store.voice_transcript(&panel_id).await {
        Ok(Some((panel, entries, threads))) => Json(json!({
            "id": panel.id,
            "startedAtMs": panel.opened_at_ms,
            "endedAtMs": panel.ended_at_ms,
            "threadIds": threads,
            "entries": entries.iter().map(|entry| json!({
                "id": entry.item_id,
                "speaker": entry.speaker,
                "text": entry.text,
                "atMs": entry.at_ms,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Ok(None) => failure(
            StatusCode::NOT_FOUND,
            "This voice transcript was deleted or never saved.",
        ),
        Err(_) => store_failure(),
    }
}

/// `DELETE /api/v1/voice/transcripts`: deletes every saved voice transcript.
pub(crate) async fn delete_transcripts(
    State(state): State<AppState>,
    Extension(_owner): Extension<OwnerAuthority>,
) -> Response {
    match state.store.delete_voice_transcripts().await {
        Ok(sessions) => {
            let _ = state.logger.record(
                "info",
                "voice.transcripts_deleted",
                json!({ "sessions": sessions }),
            );
            Json(json!({ "deletedSessions": sessions })).into_response()
        }
        Err(_) => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Voice transcripts could not be deleted. Try again.",
        ),
    }
}

#[cfg(all(test, unix))]
mod tests {
    // Contract: the owner's words reach the transcriber as its prompt, capped
    // at 100; no words means no prompt.
    #[test]
    fn dictionary_becomes_transcription_prompt() {
        let none = session_request("m", &[]);
        assert!(none["session"]["audio"]["input"]["transcription"]["prompt"].is_null());
        let words: Vec<String> = (0..150).map(|n| format!("w{n}")).collect();
        let body = session_request("m", &["T3".into(), "Cupola".into()]);
        assert_eq!(
            body["session"]["audio"]["input"]["transcription"]["prompt"],
            "The speaker may use these words and names: T3, Cupola."
        );
        let prompt = transcription_prompt(&words).unwrap();
        assert!(prompt.contains("w99") && !prompt.contains("w100"));
    }

    use super::*;
    use axum::{
        body::Body,
        http::{header, Method, Request},
        routing::{get, post},
        Router,
    };
    use tower::ServiceExt;

    const KEY: &str = "sk-test-not-a-real-key";
    const PANEL: &str = "panel-0001";

    /// Endpoint, Authorization header and JSON body of one request.
    type Call = (String, Option<String>, Option<Value>);

    #[derive(Default)]
    struct OpenAiMock {
        calls: std::sync::Mutex<Vec<Call>>,
    }

    /// A stand-in for OpenAI that accepts only `KEY`, records each request and
    /// answers like the real API, including an error that quotes the key.
    async fn mock_openai(mock: Arc<OpenAiMock>) -> String {
        async fn authorized(headers: &axum::http::HeaderMap) -> Option<String> {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        }
        let models = {
            let mock = Arc::clone(&mock);
            move |headers: axum::http::HeaderMap| {
                let mock = Arc::clone(&mock);
                async move {
                    let auth = authorized(&headers).await;
                    mock.calls
                        .lock()
                        .unwrap()
                        .push(("models".into(), auth.clone(), None));
                    if auth.as_deref() == Some(&format!("Bearer {KEY}")) {
                        (StatusCode::OK, Json(json!({"id": DEFAULT_MODEL}))).into_response()
                    } else {
                        rejection(auth)
                    }
                }
            }
        };
        let secrets = {
            let mock = Arc::clone(&mock);
            move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                let mock = Arc::clone(&mock);
                async move {
                    let auth = authorized(&headers).await;
                    mock.calls.lock().unwrap().push((
                        "client_secrets".into(),
                        auth.clone(),
                        Some(body),
                    ));
                    if auth.as_deref() == Some(&format!("Bearer {KEY}")) {
                        Json(json!({"value": "ek_shortlived", "expires_at": 1_900_000_000, "session": {}}))
                            .into_response()
                    } else {
                        rejection(auth)
                    }
                }
            }
        };
        fn rejection(auth: Option<String>) -> Response {
            // OpenAI's real 401 quotes a masked fragment of the key it got.
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": format!("Incorrect API key provided: {}", auth.unwrap_or_default())}})),
            )
                .into_response()
        }
        let app = Router::new()
            .route("/v1/models/{model}", get(models))
            .route("/v1/realtime/client_secrets", post(secrets));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    async fn fixture() -> (
        tempfile::TempDir,
        AppState,
        Arc<OpenAiMock>,
        Arc<MemoryKeyStore>,
    ) {
        let (dir, mut state) = crate::ingestion::tests::fixture().await;
        let mock = Arc::new(OpenAiMock::default());
        let base = mock_openai(Arc::clone(&mock)).await;
        let keys = Arc::new(MemoryKeyStore::default());
        state.voice = Arc::new(Voice::with_api_base(
            keys.clone(),
            base,
            DEFAULT_MODEL.into(),
        ));
        (dir, state, mock, keys)
    }

    async fn call(
        state: &AppState,
        method: Method,
        path: &str,
        body: Option<Value>,
        local: bool,
    ) -> (StatusCode, String) {
        let mut request = Request::builder().method(method).uri(path);
        if local {
            request = request.header("x-wonder-loopback-capability", "test");
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = crate::router(state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn mint(state: &AppState, panel: &str) -> (StatusCode, String) {
        call(
            state,
            Method::POST,
            "/api/v1/voice/sessions",
            Some(json!({ "panelId": panel })),
            true,
        )
        .await
    }

    fn logs(dir: &tempfile::TempDir) -> String {
        let mut text = String::new();
        for entry in std::fs::read_dir(dir.path().join("logs"))
            .into_iter()
            .flatten()
        {
            text += &std::fs::read_to_string(entry.unwrap().path()).unwrap_or_default();
        }
        text
    }

    // Contract: every voice route requires the owner (this Mac's loopback
    // capability or a paired device session); none answers anonymously.
    #[tokio::test]
    async fn voice_routes_require_an_owner() {
        let (_dir, state, mock, keys) = fixture().await;
        keys.write(KEY).unwrap();
        for (method, path, body) in [
            (Method::GET, "/api/v1/voice", None),
            (
                Method::POST,
                "/api/v1/voice/sessions",
                Some(json!({"panelId": PANEL})),
            ),
            (
                Method::PUT,
                "/api/v1/voice/openai-key",
                Some(json!({"apiKey": KEY})),
            ),
            (Method::DELETE, "/api/v1/voice/openai-key", None),
        ] {
            let (status, text) = call(&state, method.clone(), path, body, false).await;
            assert!(
                status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
                "{method} {path} answered {status} without an owner"
            );
            assert!(!text.contains("ek_") && !text.contains(KEY));
        }
        assert!(
            mock.calls.lock().unwrap().is_empty(),
            "nothing reached OpenAI"
        );
        assert_eq!(keys.read().unwrap().as_deref(), Some(KEY), "key untouched");
    }

    // Contract: the key goes into the store after OpenAI accepts it, minting
    // returns only an ephemeral secret, and neither responses nor logs carry
    // the key.
    #[tokio::test]
    async fn saved_key_mints_ephemeral_secrets_without_leaking() {
        let (dir, state, mock, keys) = fixture().await;
        let (status, text) = call(&state, Method::GET, "/api/v1/voice", None, true).await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains(r#""configured":false"#));

        let (status, text) = mint(&state, PANEL).await;
        assert_eq!(status, StatusCode::CONFLICT, "no key yet: {text}");
        assert!(text.contains("Add an OpenAI API key"), "{text}");

        let (status, text) = call(
            &state,
            Method::PUT,
            "/api/v1/voice/openai-key",
            Some(json!({"apiKey": format!("  {KEY}\n")})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert!(text.contains(r#""configured":true"#));
        assert!(!text.contains(KEY));
        crate::tests::validate_http_contract("voiceStatus", &serde_json::from_str(&text).unwrap());
        assert_eq!(
            keys.read().unwrap().as_deref(),
            Some(KEY),
            "trimmed key stored"
        );

        let (status, text) = mint(&state, PANEL).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        crate::tests::validate_http_contract("voiceSession", &body);
        assert_eq!(body["clientSecret"], "ek_shortlived");
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(
            body["url"],
            format!("wss://api.openai.com/v1/realtime?model={DEFAULT_MODEL}")
        );
        assert!(!text.contains(KEY));

        let calls = mock.calls.lock().unwrap().clone();
        let (_, auth, request) = calls
            .iter()
            .find(|(name, _, _)| name == "client_secrets")
            .expect("minted with OpenAI");
        assert_eq!(auth.as_deref(), Some(format!("Bearer {KEY}").as_str()));
        let request = request.as_ref().unwrap();
        assert_eq!(request["expires_after"]["seconds"], CLIENT_SECRET_SECONDS);
        assert_eq!(request["session"]["type"], "realtime");
        assert_eq!(request["session"]["model"], DEFAULT_MODEL);
        // The minted session must finish spoken commands automatically with
        // the measured prompt-turn setting, not the slower default timeout.
        let detection = &request["session"]["audio"]["input"]["turn_detection"];
        assert_eq!(detection["type"], "semantic_vad");
        assert_eq!(detection["eagerness"], "high");
        assert_eq!(detection["create_response"], true);

        let (status, text) = call(
            &state,
            Method::DELETE,
            "/api/v1/voice/openai-key",
            None,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(text.contains(r#""configured":false"#));
        assert_eq!(keys.read().unwrap(), None);

        let log = logs(&dir);
        assert!(log.contains("voice.session_minted"), "minting is logged");
        assert!(
            !log.contains(KEY) && !log.contains("sk-proj"),
            "key in logs"
        );
        assert!(!log.contains("ek_shortlived"), "secret in logs");
    }

    // Contract: a key OpenAI refuses is not stored, and OpenAI's error text,
    // which quotes the key, is not passed back.
    #[tokio::test]
    async fn rejected_keys_are_not_stored_or_echoed() {
        let (dir, state, _mock, keys) = fixture().await;
        let wrong = "sk-test-refused-by-api";
        let (status, text) = call(
            &state,
            Method::PUT,
            "/api/v1/voice/openai-key",
            Some(json!({"apiKey": wrong})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(text.contains("did not accept"), "{text}");
        assert!(!text.contains(wrong) && !text.contains("Incorrect API key"));
        assert_eq!(keys.read().unwrap(), None);

        let (status, _) = call(
            &state,
            Method::PUT,
            "/api/v1/voice/openai-key",
            Some(json!({"apiKey": "not a key"})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        // A stored key that OpenAI later refuses: the mint fails cleanly.
        keys.write(wrong).unwrap();
        let (status, text) = mint(&state, PANEL).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(text.contains("no longer accepts"), "{text}");
        assert!(!text.contains(wrong) && !text.contains("Incorrect API key"));
        assert!(!logs(&dir).contains(wrong));
    }

    /// Calls a handler as a paired device would reach it.
    fn paired(device: &str) -> Option<Extension<AuthenticatedDevice>> {
        Some(Extension(AuthenticatedDevice {
            device_id: device.into(),
            session_binding: String::new(),
        }))
    }

    async fn device_mint(state: &AppState, device: &str, body: Value) -> (StatusCode, String) {
        let response = create_session(
            State(state.clone()),
            Extension(OwnerAuthority),
            paired(device),
            Bytes::from(body.to_string()),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn mints_sent(mock: &OpenAiMock) -> usize {
        mock.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _, _)| name == "client_secrets")
            .count()
    }

    fn fixture_json() -> Value {
        serde_json::from_str(include_str!(
            "../../../packages/protocol/fixtures/voice-v1.json"
        ))
        .unwrap()
    }

    // Contract: a secret is minted only for an open voice panel of the device
    // asking, within the panel's session limit and reconnect allowance; each
    // refusal reaches neither OpenAI nor the logs' secrets and says why.
    #[tokio::test]
    async fn mints_need_an_open_panel_within_its_limit() {
        let (dir, state, mock, keys) = fixture().await;
        keys.write(KEY).unwrap();

        for body in [
            json!({}),
            json!({"panelId": "short"}),
            json!({"panelId": "panel 0001"}),
        ] {
            let (status, text) = device_mint(&state, "phone", body).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(text.contains("Update Cupola on this phone"), "{text}");
        }
        assert_eq!(mints_sent(&mock), 0);

        let (status, text) = device_mint(&state, "phone", json!({"panelId": PANEL})).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        crate::tests::validate_http_contract("voiceSession", &body);
        assert_eq!(body["panelId"], PANEL);
        assert_eq!(body["sessionLimitSeconds"], 15 * 60);
        assert_eq!(body["sessionEndsFor"], "sessionLimit");
        let ends = body["sessionEndsAt"].as_i64().unwrap();
        let now = crate::now_ms() as i64 / 1000;
        assert!((now + 899..=now + 901).contains(&ends), "{ends} vs {now}");

        // Another device cannot use this panel.
        let (status, text) = device_mint(&state, "tablet", json!({"panelId": PANEL})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(text.contains("another device"), "{text}");

        // A reconnect keeps the panel's end; the sixth reconnect is refused.
        for _ in 1..PANEL_MINTS_MAX {
            let (status, text) = device_mint(&state, "phone", json!({"panelId": PANEL})).await;
            assert_eq!(status, StatusCode::OK, "{text}");
            let body: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(body["sessionEndsAt"], ends);
        }
        let (status, text) = device_mint(&state, "phone", json!({"panelId": PANEL})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(text.contains("reconnected too many times"), "{text}");

        // A closed panel mints nothing more.
        let second = "panel-0002";
        state.voice.mints.lock().await.clear();
        let (status, _) = device_mint(&state, "phone", json!({"panelId": second})).await;
        assert_eq!(status, StatusCode::OK);
        let ended = end_panel(
            State(state.clone()),
            Extension(OwnerAuthority),
            paired("tablet"),
            Path(second.into()),
        )
        .await;
        assert_eq!(
            ended.status(),
            StatusCode::NOT_FOUND,
            "only its device ends it"
        );
        let ended = end_panel(
            State(state.clone()),
            Extension(OwnerAuthority),
            paired("phone"),
            Path(second.into()),
        )
        .await;
        assert_eq!(ended.status(), StatusCode::NO_CONTENT);
        let (status, text) = device_mint(&state, "phone", json!({"panelId": second})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(text.contains("has ended"), "{text}");

        // A panel past its session limit mints nothing more.
        let now_ms = crate::now_ms() as i64;
        state
            .store
            .record_voice_mint("panel-0003", "phone", now_ms - 16 * 60_000, now_ms - 60_000)
            .await
            .unwrap();
        let (status, text) = device_mint(&state, "phone", json!({"panelId": "panel-0003"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(text.contains("reached its 15-minute limit"), "{text}");

        assert_eq!(mints_sent(&mock), PANEL_MINTS_MAX as usize + 1);
        let log = logs(&dir);
        for reason in [
            "invalid_request",
            "other_device",
            "panel_mints",
            "panel_ended",
            "session_limit",
        ] {
            assert!(
                log.contains(&format!(r#""reason":"{reason}""#)),
                "{reason} logged"
            );
        }
        assert!(
            log.contains(r#""panel":"panel-0001""#),
            "mint names its panel"
        );
        assert!(!log.contains("ek_shortlived") && !log.contains(KEY));
    }

    // Contract: one device mints at most DEVICE_MINTS_MAX secrets per window,
    // counting every request that reached OpenAI; other devices are not held
    // back by it.
    #[tokio::test]
    async fn mints_are_rate_limited_per_device() {
        let (dir, state, mock, keys) = fixture().await;
        keys.write(KEY).unwrap();
        for n in 0..DEVICE_MINTS_MAX {
            let (status, text) = device_mint(
                &state,
                "phone",
                json!({"panelId": format!("panel-a{n:04}")}),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{text}");
        }
        let (status, text) = device_mint(&state, "phone", json!({"panelId": "panel-a9999"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            text.contains("too many times in the last few minutes"),
            "{text}"
        );
        let (status, _) = device_mint(&state, "tablet", json!({"panelId": "panel-b0001"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mints_sent(&mock), DEVICE_MINTS_MAX + 1);
        assert!(logs(&dir).contains(r#""reason":"rate_limited""#));
    }

    async fn put_settings(state: &AppState, change: Value) -> (StatusCode, Value) {
        let (status, text) = call(
            state,
            Method::PUT,
            "/api/v1/voice/settings",
            Some(change),
            true,
        )
        .await;
        (status, serde_json::from_str(&text).unwrap_or(json!(text)))
    }

    async fn report(state: &AppState, panel: &str, body: Value) -> (StatusCode, Value) {
        let (status, text) = call(
            state,
            Method::POST,
            &format!("/api/v1/voice/panels/{panel}/usage"),
            Some(body),
            true,
        )
        .await;
        (status, serde_json::from_str(&text).unwrap_or(json!(text)))
    }

    // Contract: the owner's daily minutes and dollars are enforced at mint
    // from the usage the phone reported, with a reason the phone shows; the
    // mint tells the phone to end early when today's minutes run out first.
    #[tokio::test]
    async fn daily_budget_refuses_mints_with_a_reason() {
        let (dir, state, _mock, keys) = fixture().await;
        keys.write(KEY).unwrap();
        let (status, settings) = put_settings(&state, json!({"dailyMinutes": 2})).await;
        assert_eq!(status, StatusCode::OK, "{settings}");

        assert_eq!(mint(&state, PANEL).await.0, StatusCode::OK);
        let (status, result) = report(&state, PANEL, json!({"connectedSeconds": 60})).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        crate::tests::validate_http_contract("voiceUsageResult", &result);
        assert_eq!(result["today"]["spokenSeconds"], 60);

        // One minute left: the next session ends for the daily limit.
        let (status, text) = mint(&state, "panel-0002").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["sessionEndsFor"], "dailyLimit");
        let now = crate::now_ms() as i64 / 1000;
        let ends = body["sessionEndsAt"].as_i64().unwrap();
        assert!((now + 59..=now + 61).contains(&ends), "{ends} vs {now}");

        // A smaller running total never lowers the count.
        report(&state, "panel-0002", json!({"connectedSeconds": 60})).await;
        report(&state, "panel-0002", json!({"connectedSeconds": 30})).await;
        let (status, text) = mint(&state, "panel-0003").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            text.contains("Today's voice limit of 2 minutes is used up"),
            "{text}"
        );

        // Dollars: the fixture's usage costs about $0.0633.
        put_settings(&state, json!({"dailyMinutes": null, "dailyDollars": 0.05})).await;
        assert_eq!(mint(&state, "panel-0004").await.0, StatusCode::OK);
        let usage = fixture_json()["voiceUsageReport"].clone();
        for _ in 0..2 {
            let (status, result) = report(&state, "panel-0004", usage.clone()).await;
            assert_eq!(status, StatusCode::OK, "{result}");
        }
        let (_, settings) = call(&state, Method::GET, "/api/v1/voice/settings", None, true).await;
        let settings: Value = serde_json::from_str(&settings).unwrap();
        assert_eq!(
            settings["today"]["estimatedDollars"], 0.07,
            "events count once"
        );
        let (status, text) = mint(&state, "panel-0005").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            text.contains("spending limit of $0.05 is used up"),
            "{text}"
        );
        let log = logs(&dir);
        assert!(
            log.contains(r#""reason":"daily_minutes""#) && log.contains(r#""reason":"daily_cost""#)
        );

        // Reports go only to a panel this device opened.
        let (status, _) = report(&state, "panel-none", json!({"connectedSeconds": 1})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // Contract: a panel the phone never reports on still counts the time it
    // was open on this Mac, up to its session limit.
    #[test]
    fn unreported_panels_count_their_open_time() {
        let panel = |connected, events, ended| VoicePanel {
            id: PANEL.into(),
            device_id: "phone".into(),
            opened_at_ms: 0,
            ends_at_ms: 900_000,
            ended_at_ms: ended,
            mints: 1,
            connected_seconds: connected,
            usage_events: events,
            cost_microdollars: 0,
        };
        assert_eq!(billed_seconds(&panel(0, 0, None), 120_000), 120);
        assert_eq!(billed_seconds(&panel(0, 0, Some(30_000)), 120_000), 30);
        assert_eq!(billed_seconds(&panel(0, 0, None), 5_000_000), 900);
        assert_eq!(billed_seconds(&panel(45, 0, None), 120_000), 45);
        assert_eq!(billed_seconds(&panel(0, 2, None), 120_000), 0);
    }

    // Contract: settings default to a 15-minute session, no daily limits and
    // kept transcripts; out-of-range values are refused and unchanged fields
    // stay as they were.
    #[tokio::test]
    async fn voice_settings_round_trip_with_bounds() {
        let (_dir, state, _mock, keys) = fixture().await;
        let (status, text) = call(&state, Method::GET, "/api/v1/voice/settings", None, true).await;
        assert_eq!(status, StatusCode::OK);
        let settings: Value = serde_json::from_str(&text).unwrap();
        crate::tests::validate_http_contract("voiceSettings", &settings);
        assert_eq!(settings["sessionLimitMinutes"], 15);
        assert_eq!(settings["sessionLimitMaxMinutes"], 55);
        assert_eq!(settings["dailyMinutes"], Value::Null);
        assert_eq!(settings["dailyDollars"], Value::Null);
        assert_eq!(settings["keepTranscripts"], true);

        let (status, settings) = put_settings(
            &state,
            json!({"sessionLimitMinutes": 55, "dailyMinutes": 90, "dailyDollars": 2.5}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        crate::tests::validate_http_contract("voiceSettings", &settings);
        assert_eq!(settings["dailyDollars"], 2.5);
        for (change, reason) in [
            (json!({"sessionLimitMinutes": 56}), "1 to 55 minutes"),
            (json!({"sessionLimitMinutes": 0}), "1 to 55 minutes"),
            (json!({"dailyMinutes": 0}), "daily limit"),
            (json!({"dailyDollars": 0}), "spending limit"),
        ] {
            let (status, text) = put_settings(&state, change).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
            assert!(text.to_string().contains(reason), "{text}");
        }
        let (_, settings) = put_settings(&state, json!({"dailyMinutes": null})).await;
        assert_eq!(settings["dailyMinutes"], Value::Null);
        assert_eq!(settings["sessionLimitMinutes"], 55);
        assert_eq!(settings["dailyDollars"], 2.5);

        // The mint carries the owner's session limit.
        keys.write(KEY).unwrap();
        let (_, text) = mint(&state, PANEL).await;
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["sessionLimitSeconds"], 55 * 60);
    }

    // Contract: transcripts (text only) are kept on this Mac linked to the
    // threads the session acted on while the setting is on, not when it is
    // off, and deleting them all leaves today's usage counted.
    #[tokio::test]
    async fn transcripts_are_kept_linked_and_deleted() {
        let (_dir, state, _mock, keys) = fixture().await;
        keys.write(KEY).unwrap();
        let fixtures = fixture_json();
        assert_eq!(mint(&state, PANEL).await.0, StatusCode::OK);
        report(&state, PANEL, json!({"connectedSeconds": 40})).await;
        let path = format!("/api/v1/voice/panels/{PANEL}/transcript");
        let (status, text) = call(
            &state,
            Method::POST,
            &path,
            Some(fixtures["voiceTranscriptReport"].clone()),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(text, r#"{"saved":true}"#);

        let (_, text) = call(
            &state,
            Method::GET,
            "/api/v1/voice/transcripts?threadId=conversation-release",
            None,
            true,
        )
        .await;
        let list: Value = serde_json::from_str(&text).unwrap();
        crate::tests::validate_http_contract("voiceTranscriptList", &list);
        assert_eq!(list["sessions"][0]["id"], PANEL);
        assert_eq!(
            list["sessions"][0]["preview"],
            "Tell the release chat the build is green."
        );
        assert_eq!(
            list["sessions"][0]["threadIds"],
            json!(["conversation-release"])
        );
        let (_, text) = call(
            &state,
            Method::GET,
            "/api/v1/voice/transcripts?threadId=other",
            None,
            true,
        )
        .await;
        assert_eq!(text, r#"{"sessions":[]}"#);
        let (_, text) = call(
            &state,
            Method::GET,
            &format!("/api/v1/voice/transcripts/{PANEL}"),
            None,
            true,
        )
        .await;
        let saved: Value = serde_json::from_str(&text).unwrap();
        crate::tests::validate_http_contract("voiceTranscript", &saved);
        assert_eq!(
            saved["entries"],
            fixtures["voiceTranscriptReport"]["entries"]
        );

        // Off: nothing more is kept.
        put_settings(&state, json!({"keepTranscripts": false})).await;
        let (status, text) = call(
            &state,
            Method::POST,
            &path,
            Some(json!({"entries": [{"id": "later", "speaker": "owner", "text": "Not kept", "atMs": 1}]})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(text, r#"{"saved":false}"#);
        let (_, text) = call(
            &state,
            Method::GET,
            &format!("/api/v1/voice/transcripts/{PANEL}"),
            None,
            true,
        )
        .await;
        assert!(!text.contains("Not kept"));

        let (status, text) = call(
            &state,
            Method::DELETE,
            "/api/v1/voice/transcripts",
            None,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(text, r#"{"deletedSessions":1}"#);
        let (_, text) = call(&state, Method::GET, "/api/v1/voice/transcripts", None, true).await;
        assert_eq!(text, r#"{"sessions":[]}"#);
        let (status, _) = call(
            &state,
            Method::GET,
            &format!("/api/v1/voice/transcripts/{PANEL}"),
            None,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, text) = call(&state, Method::GET, "/api/v1/voice/settings", None, true).await;
        assert!(text.contains(r#""spokenSeconds":40"#), "{text}");

        // Malformed transcripts are refused.
        put_settings(&state, json!({"keepTranscripts": true})).await;
        let (status, _) = call(
            &state,
            Method::POST,
            &path,
            Some(json!({"entries": [{"id": "x", "speaker": "someone", "text": "", "atMs": 1}]})),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Contract: every example in the voice fixture is valid under the HTTP
    // schema, and the fixture's usage prices at the published rates.
    #[test]
    fn voice_fixture_matches_the_schema() {
        let fixtures = fixture_json();
        for (definition, example) in fixtures.as_object().unwrap() {
            crate::tests::validate_http_contract(definition, example);
        }
        let events = fixtures["voiceUsageReport"]["events"].as_array().unwrap();
        let total: i64 = events
            .iter()
            .map(|event| usage_cost(event["source"].as_str().unwrap(), &event["usage"]).unwrap())
            .sum();
        // Text in 200×$4 + cached 800×$0.40 + audio in 500×$32 + text out
        // 50×$24 + audio out 700×$64 per 1M, plus 4 s of transcription.
        assert_eq!(total, 800 + 320 + 16_000 + 1_200 + 44_800 + 200);
        assert_eq!(usage_cost("unknown", &json!({})), None);
    }

    #[test]
    fn key_and_model_shapes() {
        assert!(plausible_key(KEY));
        assert!(!plausible_key("sk-short"));
        assert!(!plausible_key("pk-0123456789012345678901234"));
        assert!(!plausible_key("sk-0123456789 0123456789"));
        assert!(valid_model("gpt-realtime-2.1-mini"));
        assert!(!valid_model("gpt?x=1"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn file_key_store_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = KeychainKeyStore::for_data_home(dir.path());
        assert_eq!(store.read().unwrap(), None);
        store.write(KEY).unwrap();
        store.write(KEY).unwrap();
        assert_eq!(store.read().unwrap().as_deref(), Some(KEY));
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let file = dir.path().join("secrets/voice-openai-key");
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
        store.remove().unwrap();
        store.remove().unwrap();
        assert_eq!(store.read().unwrap(), None);
    }
}
