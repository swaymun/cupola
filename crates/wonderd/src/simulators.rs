//! Typed settings for this Mac's simulators: list, appearance, text size,
//! language, app permissions, links and screenshots. The paired phone's
//! Simulator view and outside agents (`mcp_http`) both use these actions.
//!
//! Every action is one fixed `xcrun simctl` argument vector (never a shell),
//! with validated arguments, a timeout and bounded output, and reads its
//! result back where simctl can. Settings actions only touch a simulator that
//! is already booted on this Mac; `boot` and `shutdown` start and stop one.
//! Input (touch, buttons, typing) is not here: it stays behind a computer
//! session's control lease (`computer_sessions`).
//!
//! Cupola's own test simulators ("Wonder Control …") are refused while a
//! `scripts/wonderctl` lease is active, and the nightly's copies always, so
//! nobody changes a device under a running scenario; boot and shutdown refuse
//! every "Wonder Control" device, since pooled lanes hold no lease. Listing
//! still shows them.
use crate::{AppState, AuthenticatedDevice};
use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub(crate) const XCRUN: &str = "/usr/bin/xcrun";
pub(crate) const PLUTIL: &str = "/usr/bin/plutil";
pub(crate) const CODESIGN: &str = "/usr/bin/codesign";
/// The Duo posture helper scripts/wonderctl's `simrig` builds, compiled here
/// once from the same source and run inside the simulator.
const RIG_SOURCE: &str = include_str!("../../../apps/ios/control/rig/main.m");
const RIG_ENTITLEMENTS: &str = include_str!("../../../apps/ios/control/rig/rig.entitlements");
/// What the rig accepts for `orient` (the Duo's screens swap by turning 180°).
pub(crate) const ORIENTATIONS: [&str; 6] = [
    "portrait",
    "pud",
    "landscape-left",
    "landscape-right",
    "faceup",
    "facedown",
];
/// One simctl call; a hung CoreSimulator never holds a request longer.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Waiting for a started simulator to finish booting.
pub(crate) const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
/// The posture helper waits for the simulator's virtual device to attach.
const RIG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Base64 twice (here, then in a sealed relay answer) stays under 16 MB.
pub(crate) const MAX_SCREENSHOT_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_URL_BYTES: usize = 2048;
const MAX_LISTED: usize = 200;
const MAX_APPS: usize = 200;
/// Concurrent simctl processes per host.
const MAX_CALLS: usize = 4;

pub(crate) const APPEARANCES: [&str; 2] = ["light", "dark"];
/// The UIContentSizeCategory names `simctl ui <udid> content_size` accepts.
pub(crate) const CONTENT_SIZES: [&str; 12] = [
    "extra-small",
    "small",
    "medium",
    "large",
    "extra-large",
    "extra-extra-large",
    "extra-extra-extra-large",
    "accessibility-medium",
    "accessibility-large",
    "accessibility-extra-large",
    "accessibility-extra-extra-large",
    "accessibility-extra-extra-extra-large",
];
/// Schemes that read local files or run script in whatever opens them.
const REFUSED_SCHEMES: [&str; 8] = [
    "file",
    "javascript",
    "data",
    "blob",
    "about",
    "vbscript",
    "filesystem",
    "view-source",
];
const TEST_DEVICE_PREFIX: &str = "Wonder Control ";
const NIGHTLY_DEVICE_SUFFIX: &str = " Nightly";

pub(crate) const LANGUAGE_RESTART_MESSAGE: &str = "Apps use the new language the next time they open. The Home Screen and Settings change after the simulator restarts.";
/// Outside agents get settings and screenshots; touch input needs the phone's
/// control lease, which an agent cannot hold.
pub(crate) const AGENT_INPUT_UNAVAILABLE: &str = "Agents can't tap, swipe or type in simulators. They can list simulators, take screenshots and change settings; to use a simulator by touch, open it in Cupola's Simulator view on your phone.";

/// One process to run: a fixed program and its exact argument vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub program: &'static str,
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Ran {
    pub success: bool,
    pub stdout: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunFailure {
    Missing,
    TimedOut,
    TooLarge,
    Io,
}

/// Runs simctl/plutil. Tests substitute a recorder; the host runs processes.
pub(crate) trait Runner: Send + Sync {
    fn run(&self, invocation: Invocation) -> BoxFuture<'_, Result<Ran, RunFailure>>;
}

struct Processes;

async fn read_bounded<R: AsyncRead + Unpin>(
    reader: Option<R>,
    cap: usize,
) -> Result<Vec<u8>, RunFailure> {
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    reader
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| RunFailure::Io)?;
    if bytes.len() > cap {
        return Err(RunFailure::TooLarge);
    }
    Ok(bytes)
}

impl Runner for Processes {
    fn run(&self, invocation: Invocation) -> BoxFuture<'_, Result<Ran, RunFailure>> {
        Box::pin(async move {
            let mut command = tokio::process::Command::new(invocation.program);
            command
                .args(&invocation.args)
                .stdin(if invocation.stdin.is_some() {
                    std::process::Stdio::piped()
                } else {
                    std::process::Stdio::null()
                })
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let mut child = command.spawn().map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    RunFailure::Missing
                } else {
                    RunFailure::Io
                }
            })?;
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let input = invocation.stdin;
            let timeout = invocation.timeout;
            let work = async {
                let write = async {
                    if let (Some(mut pipe), Some(input)) = (stdin, input) {
                        // A closed pipe shows up as the child's own failure.
                        let _ = pipe.write_all(&input).await;
                    }
                };
                let ((), stdout) = tokio::join!(write, read_bounded(stdout, MAX_OUTPUT_BYTES));
                let status = child.wait().await.map_err(|_| RunFailure::Io)?;
                Ok(Ran {
                    success: status.success(),
                    stdout: stdout?,
                })
            };
            // On timeout or overflow the child is dropped, which kills it.
            tokio::time::timeout(timeout, work)
                .await
                .map_err(|_| RunFailure::TimedOut)?
        })
    }
}

/// Why an action was refused or failed, in words for the person using it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SimError {
    Invalid(&'static str),
    Unavailable(&'static str),
    NotFound,
    NotBooted,
    InUseByTests,
    AppNotInstalled,
    Failed(&'static str),
}

impl SimError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::NotBooted | Self::InUseByTests => StatusCode::CONFLICT,
            Self::AppNotInstalled => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Failed(_) => StatusCode::BAD_GATEWAY,
        }
    }
    /// Stable for clients (`x-wonder-error`, MCP error codes).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::Unavailable(_) => "simulator-unavailable",
            Self::NotFound => "simulator-not-found",
            Self::NotBooted => "simulator-not-booted",
            Self::InUseByTests => "simulator-in-use-by-tests",
            Self::AppNotInstalled => "app-not-installed",
            Self::Failed(_) => "simulator-action-failed",
        }
    }
    pub fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) | Self::Unavailable(message) | Self::Failed(message) => message,
            Self::NotFound => "This simulator isn't on this computer anymore.",
            Self::NotBooted => {
                "This simulator isn't running. Start it on the computer, then try again."
            }
            Self::InUseByTests => "This simulator is being used for tests right now.",
            Self::AppNotInstalled => "That app isn't installed in this simulator.",
        }
    }
}

impl IntoResponse for SimError {
    fn into_response(self) -> Response {
        let mut response = (self.status(), self.message()).into_response();
        response.headers_mut().insert(
            crate::projects::ERROR_CODE_HEADER,
            HeaderValue::from_static(self.code()),
        );
        response
    }
}

const NO_XCODE: &str = "Xcode isn't set up on this computer, so its simulators can't be used.";
const NOT_A_MAC: &str = "Simulators work on a Mac with Xcode.";
const TIMED_OUT: &str = "The simulator didn't answer in time. Try again.";
const BUSY: &str = "The simulator is busy with other requests. Try again in a moment.";

fn run_failure(failure: RunFailure) -> SimError {
    match failure {
        RunFailure::Missing => SimError::Unavailable(NO_XCODE),
        RunFailure::TimedOut => SimError::Failed(TIMED_OUT),
        RunFailure::TooLarge => SimError::Failed("The simulator's answer was too large."),
        RunFailure::Io => SimError::Failed("The simulator couldn't be reached. Try again."),
    }
}

/// One simulator as the phone's device picker shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device {
    pub udid: String,
    pub name: String,
    pub runtime: String,
    pub state: &'static str,
    pub device_type: String,
    pub in_use_by_tests: bool,
}

impl Device {
    pub fn json(&self) -> Value {
        json!({
            "udid": self.udid,
            "sourceId": format!("simulator:{}", self.udid),
            "name": self.name,
            "runtime": self.runtime,
            "state": self.state,
            "deviceType": self.device_type,
            "inUseByTests": self.in_use_by_tests,
            "hasHinge": self.has_hinge(),
            "canStop": self.can_stop(),
        })
    }

    /// Start and stop refuse Cupola's own test simulators, leased or not.
    pub fn can_stop(&self) -> bool {
        !self.in_use_by_tests && !self.name.starts_with(TEST_DEVICE_PREFIX)
    }

    /// Only the iPhone Duo simulator has a hinge to move.
    pub fn has_hinge(&self) -> bool {
        self.device_type.contains("Duo")
    }
}

pub(crate) struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub sha256: String,
}

impl Screenshot {
    /// Metadata without the image bytes.
    pub fn summary(&self, udid: &str) -> Value {
        json!({
            "udid": udid,
            "mimeType": "image/png",
            "byteCount": self.png.len(),
            "width": self.width,
            "height": self.height,
            "sha256": self.sha256,
        })
    }
    pub fn json(&self, udid: &str) -> Value {
        use base64::Engine as _;
        let mut value = self.summary(udid);
        value["imageBase64"] = json!(base64::engine::general_purpose::STANDARD.encode(&self.png));
        value
    }
}

pub struct Simulators {
    runner: Option<Arc<dyn Runner>>,
    /// `scripts/wonderctl`'s one-simulator-at-a-time lease on this Mac.
    lease_file: Option<PathBuf>,
    /// Where the compiled posture helper lives; none disables hinge control.
    rig_dir: Option<PathBuf>,
    /// CoreSimulator's folder of device data; a posture helper is only ever
    /// copied into the target simulator's own `data/tmp` below it.
    devices_root: Option<PathBuf>,
    rig_build: tokio::sync::Mutex<()>,
    calls: tokio::sync::Semaphore,
}

impl Default for Simulators {
    fn default() -> Self {
        Self::disabled()
    }
}

impl Simulators {
    /// The host's own simctl on macOS; unavailable elsewhere.
    pub fn on_this_mac() -> Self {
        if !cfg!(target_os = "macos") {
            return Self::disabled();
        }
        let lease_file = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Caches/wonderctl/simulator-lease.json"));
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self::new(Some(Arc::new(Processes)), lease_file).with_rig(
            home.as_ref()
                .map(|home| home.join("Library/Caches/wonderd/simrig")),
            home.map(|home| home.join("Library/Developer/CoreSimulator/Devices")),
        )
    }

    pub(crate) fn with_rig(
        mut self,
        rig_dir: Option<PathBuf>,
        devices_root: Option<PathBuf>,
    ) -> Self {
        self.rig_dir = rig_dir;
        self.devices_root = devices_root;
        self
    }

    pub fn disabled() -> Self {
        Self::new(None, None)
    }

    pub(crate) fn new(runner: Option<Arc<dyn Runner>>, lease_file: Option<PathBuf>) -> Self {
        Self {
            runner,
            lease_file,
            rig_dir: None,
            devices_root: None,
            rig_build: tokio::sync::Mutex::new(()),
            calls: tokio::sync::Semaphore::new(MAX_CALLS),
        }
    }

    async fn run(
        &self,
        program: &'static str,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<Ran, SimError> {
        let Some(runner) = self.runner.as_ref() else {
            return Err(SimError::Unavailable(NOT_A_MAC));
        };
        let _permit = tokio::time::timeout(CALL_TIMEOUT, self.calls.acquire())
            .await
            .map_err(|_| SimError::Failed(BUSY))?
            .map_err(|_| SimError::Failed(BUSY))?;
        runner
            .run(Invocation {
                program,
                args,
                stdin,
                timeout,
            })
            .await
            .map_err(run_failure)
    }

    async fn simctl(&self, args: &[&str]) -> Result<Ran, SimError> {
        let mut argv = vec!["simctl".to_owned()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.run(XCRUN, argv, None, CALL_TIMEOUT).await
    }

    fn test_lease_active(&self) -> bool {
        let Some(path) = self.lease_file.as_ref() else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        lease_active(&text, chrono::Local::now().naive_local())
    }

    /// Every simulator this Mac's Xcode can run, booted first.
    pub(crate) async fn list(&self) -> Result<Vec<Device>, SimError> {
        let ran = self.simctl(&["list", "--json"]).await?;
        if !ran.success {
            return Err(SimError::Unavailable(NO_XCODE));
        }
        let listing: Value =
            serde_json::from_slice(&ran.stdout).map_err(|_| SimError::Unavailable(NO_XCODE))?;
        Ok(parse_devices(&listing, self.test_lease_active()))
    }

    /// The device, only when it is booted here and free to change.
    async fn booted(&self, udid: &str) -> Result<String, SimError> {
        let udid = valid_udid(udid).ok_or(SimError::Invalid("That isn't a simulator ID."))?;
        let device = self
            .list()
            .await?
            .into_iter()
            .find(|device| device.udid == udid)
            .ok_or(SimError::NotFound)?;
        if device.state != "booted" {
            return Err(SimError::NotBooted);
        }
        if device.in_use_by_tests {
            return Err(SimError::InUseByTests);
        }
        Ok(udid)
    }

    /// The device to start or stop: never one of Cupola's test simulators,
    /// leased or not (pooled lanes hold no lease).
    async fn startable(&self, udid: &str) -> Result<Device, SimError> {
        let udid = valid_udid(udid).ok_or(SimError::Invalid("That isn't a simulator ID."))?;
        let device = self
            .list()
            .await?
            .into_iter()
            .find(|device| device.udid == udid)
            .ok_or(SimError::NotFound)?;
        if device.in_use_by_tests || device.name.starts_with(TEST_DEVICE_PREFIX) {
            return Err(SimError::InUseByTests);
        }
        Ok(device)
    }

    async fn state_of(&self, udid: &str) -> Result<&'static str, SimError> {
        let devices = self.list().await?;
        Ok(devices
            .iter()
            .find(|device| device.udid == udid)
            .map_or("unknown", |device| device.state))
    }

    /// Starts a stopped simulator and waits until it has finished booting. A
    /// running one is a success too: the answer is its state.
    pub(crate) async fn boot(&self, udid: &str) -> Result<Value, SimError> {
        let device = self.startable(udid).await?;
        let udid = device.udid;
        if device.state != "booted" && !self.simctl(&["boot", &udid]).await?.success {
            // Booted or booting meanwhile (someone else started it) is fine.
            if !matches!(self.state_of(&udid).await?, "booted" | "booting") {
                return Err(SimError::Failed(
                    "The simulator couldn't be started. Try again.",
                ));
            }
        }
        let args = ["simctl", "bootstatus", &udid, "-b"]
            .map(str::to_owned)
            .to_vec();
        let waited = self.run(XCRUN, args, None, BOOT_TIMEOUT).await;
        match waited {
            Ok(ran) if ran.success => Ok(json!({"udid": udid, "state": "booted"})),
            Err(SimError::Failed(TIMED_OUT)) => Err(SimError::Failed(
                "The simulator is taking too long to start. Try again.",
            )),
            Ok(_) => Err(SimError::Failed(
                "The simulator couldn't finish starting. Try again.",
            )),
            Err(error) => Err(error),
        }
    }

    /// Stops a simulator; one that is already stopped is a success.
    pub(crate) async fn shutdown(&self, udid: &str) -> Result<Value, SimError> {
        let device = self.startable(udid).await?;
        let udid = device.udid;
        if device.state != "shutdown"
            && !self.simctl(&["shutdown", &udid]).await?.success
            && self.state_of(&udid).await? != "shutdown"
        {
            return Err(SimError::Failed(
                "The simulator couldn't be stopped. Try again.",
            ));
        }
        Ok(json!({"udid": udid, "state": "shutdown"}))
    }

    async fn read_ui(&self, udid: &str, option: &str) -> Result<String, SimError> {
        let ran = self.simctl(&["ui", udid, option]).await?;
        if !ran.success {
            return Err(SimError::Failed(
                "The simulator's settings couldn't be read.",
            ));
        }
        Ok(ui_value(&ran.stdout))
    }

    async fn read_global(&self, udid: &str, key: &str) -> Result<Option<String>, SimError> {
        let ran = self
            .simctl(&["spawn", udid, "defaults", "read", "-g", key])
            .await?;
        // A missing key exits 1; that is an unset value, not a failure.
        Ok(ran
            .success
            .then(|| String::from_utf8_lossy(&ran.stdout).trim().to_owned()))
    }

    async fn current_language(
        &self,
        udid: &str,
    ) -> Result<(Option<String>, Option<String>), SimError> {
        let languages = self.read_global(udid, "AppleLanguages").await?;
        let locale = self.read_global(udid, "AppleLocale").await?;
        Ok((
            languages.as_deref().and_then(first_language),
            locale.filter(|locale| valid_locale(locale)),
        ))
    }

    pub(crate) async fn settings(&self, udid: &str) -> Result<Value, SimError> {
        let udid = self.booted(udid).await?;
        let appearance = self.read_ui(&udid, "appearance").await?;
        let content_size = self.read_ui(&udid, "content_size").await?;
        let (language, locale) = self.current_language(&udid).await?;
        Ok(json!({
            "udid": udid,
            "appearance": appearance_value(&appearance),
            "contentSize": content_size_value(&content_size),
            "language": language,
            "locale": locale,
        }))
    }

    pub(crate) async fn set_appearance(
        &self,
        udid: &str,
        appearance: &str,
    ) -> Result<Value, SimError> {
        if !APPEARANCES.contains(&appearance) {
            return Err(SimError::Invalid("Choose Light or Dark."));
        }
        let udid = self.booted(udid).await?;
        let ran = self
            .simctl(&["ui", &udid, "appearance", appearance])
            .await?;
        if !ran.success {
            return Err(SimError::Failed(
                "The simulator couldn't change its appearance. Try again.",
            ));
        }
        let now = appearance_value(&self.read_ui(&udid, "appearance").await?);
        Ok(json!({"udid": udid, "appearance": now, "applied": now == appearance}))
    }

    pub(crate) async fn set_content_size(&self, udid: &str, size: &str) -> Result<Value, SimError> {
        if !CONTENT_SIZES.contains(&size) {
            return Err(SimError::Invalid("Choose one of the listed text sizes."));
        }
        let udid = self.booted(udid).await?;
        let ran = self.simctl(&["ui", &udid, "content_size", size]).await?;
        if !ran.success {
            return Err(SimError::Failed(
                "The simulator couldn't change its text size. Try again.",
            ));
        }
        let now = content_size_value(&self.read_ui(&udid, "content_size").await?);
        Ok(json!({"udid": udid, "contentSize": now, "applied": now == size}))
    }

    /// Writes the device's global AppleLanguages and AppleLocale. Running
    /// apps and the system keep the old language until they restart.
    pub(crate) async fn set_language(&self, udid: &str, language: &str) -> Result<Value, SimError> {
        let Some(locale) = locale_for(language) else {
            return Err(SimError::Invalid(
                "Choose a language such as en, fr-FR or zh-Hans-CN.",
            ));
        };
        let udid = self.booted(udid).await?;
        for args in [
            [
                "spawn",
                udid.as_str(),
                "defaults",
                "write",
                "-g",
                "AppleLanguages",
                "-array",
                language,
            ],
            [
                "spawn",
                udid.as_str(),
                "defaults",
                "write",
                "-g",
                "AppleLocale",
                "-string",
                locale.as_str(),
            ],
        ] {
            if !self.simctl(&args).await?.success {
                return Err(SimError::Failed(
                    "The simulator couldn't change its language. Try again.",
                ));
            }
        }
        let (now_language, now_locale) = self.current_language(&udid).await?;
        let applied =
            now_language.as_deref() == Some(language) && now_locale.as_deref() == Some(&locale);
        Ok(json!({
            "udid": udid,
            "language": now_language,
            "locale": now_locale,
            "applied": applied,
            "needsRestart": true,
            "message": LANGUAGE_RESTART_MESSAGE,
        }))
    }

    /// Apps the owner installed, for choosing which one to reset.
    pub(crate) async fn apps(&self, udid: &str) -> Result<Value, SimError> {
        let udid = self.booted(udid).await?;
        let listed = self.simctl(&["listapps", &udid]).await?;
        if !listed.success {
            return Err(SimError::Failed("The simulator's apps couldn't be read."));
        }
        // simctl prints an old-style plist; plutil turns it into JSON.
        let converted = self
            .run(
                PLUTIL,
                ["-convert", "json", "-o", "-", "--", "-"]
                    .map(str::to_owned)
                    .to_vec(),
                Some(listed.stdout),
                CALL_TIMEOUT,
            )
            .await?;
        let apps: Value = serde_json::from_slice(&converted.stdout)
            .ok()
            .filter(|_| converted.success)
            .ok_or(SimError::Failed("The simulator's apps couldn't be read."))?;
        Ok(json!({"udid": udid, "apps": user_apps(&apps)}))
    }

    /// Resets every privacy permission of one installed app, so it asks again.
    pub(crate) async fn reset_permissions(
        &self,
        udid: &str,
        bundle_id: &str,
    ) -> Result<Value, SimError> {
        if !valid_bundle_id(bundle_id) {
            return Err(SimError::Invalid("That isn't an app ID."));
        }
        let udid = self.booted(udid).await?;
        if !self
            .simctl(&["get_app_container", &udid, bundle_id])
            .await?
            .success
        {
            return Err(SimError::AppNotInstalled);
        }
        if !self
            .simctl(&["privacy", &udid, "reset", "all", bundle_id])
            .await?
            .success
        {
            return Err(SimError::Failed(
                "The app's permissions couldn't be reset. Try again.",
            ));
        }
        Ok(json!({"udid": udid, "bundleId": bundle_id, "reset": true}))
    }

    pub(crate) async fn open_url(&self, udid: &str, url: &str) -> Result<Value, SimError> {
        let url = valid_url(url).ok_or(SimError::Invalid(
            "Open a web link (http or https) or an app link such as myapp://path.",
        ))?;
        let udid = self.booted(udid).await?;
        if !self.simctl(&["openurl", &udid, &url]).await?.success {
            return Err(SimError::Failed(
                "No app in this simulator opens that link.",
            ));
        }
        Ok(json!({"udid": udid, "url": url, "opened": true}))
    }

    /// Moves a booted Duo simulator's hinge: 0 is closed, 180 flat open.
    pub(crate) async fn set_hinge(&self, udid: &str, degrees: u16) -> Result<Value, SimError> {
        if degrees > 180 {
            return Err(SimError::Invalid("The hinge opens from 0 to 180 degrees."));
        }
        let udid = self.rig_target(udid).await?;
        self.rig(&udid, &["hinge", &degrees.to_string()]).await?;
        Ok(json!({"udid": udid, "hinge": degrees}))
    }

    /// Turns a booted Duo simulator (portrait, upside down, landscape...);
    /// turning it half a turn swaps its two screens.
    pub(crate) async fn set_orientation(
        &self,
        udid: &str,
        orientation: &str,
    ) -> Result<Value, SimError> {
        if !ORIENTATIONS.contains(&orientation) {
            return Err(SimError::Invalid("That isn't a screen orientation."));
        }
        let udid = self.rig_target(udid).await?;
        self.rig(&udid, &["orient", orientation]).await?;
        Ok(json!({"udid": udid, "orientation": orientation}))
    }

    /// The booted Duo this posture change may move.
    async fn rig_target(&self, udid: &str) -> Result<String, SimError> {
        let udid = self.booted(udid).await?;
        let device = self
            .list()
            .await?
            .into_iter()
            .find(|device| device.udid == udid)
            .ok_or(SimError::NotFound)?;
        if !device.has_hinge() {
            return Err(SimError::Invalid(
                "Only the iPhone Duo simulator has a hinge.",
            ));
        }
        // A pooled test lane belongs to a running scenario; its posture isn't
        // ours to move, the same as Stop.
        if !device.can_stop() {
            return Err(SimError::Invalid(
                "This simulator is in use for tests; its hinge can't be moved from here.",
            ));
        }
        Ok(udid)
    }

    /// Builds the helper once (clang, then an ad-hoc signature; the entitlements
    /// live in `__TEXT` because launchd_sim rejects them in a signature).
    async fn rig_binary(&self) -> Result<(PathBuf, String), SimError> {
        const FAILED: &str = "The simulator's hinge couldn't be moved. Try again.";
        let Some(root) = self.rig_dir.clone() else {
            return Err(SimError::Unavailable(NOT_A_MAC));
        };
        let target = if cfg!(target_arch = "x86_64") {
            "x86_64-apple-ios17.0-simulator"
        } else {
            "arm64-apple-ios17.0-simulator"
        };
        let digest = hex::encode(Sha256::digest(
            [RIG_SOURCE, RIG_ENTITLEMENTS, target].join("\0").as_bytes(),
        ))[..16]
            .to_owned();
        let directory = root.join(&digest);
        let binary = directory.join("wonder-simrig");
        let _building = self.rig_build.lock().await;
        if tokio::fs::metadata(&binary).await.is_ok() {
            return Ok((binary, digest));
        }
        let io = |_| SimError::Failed(FAILED);
        tokio::fs::create_dir_all(&directory).await.map_err(io)?;
        let source = directory.join("main.m");
        let entitlements = directory.join("rig.entitlements");
        tokio::fs::write(&source, RIG_SOURCE).await.map_err(io)?;
        tokio::fs::write(&entitlements, RIG_ENTITLEMENTS)
            .await
            .map_err(io)?;
        let partial = directory.join("wonder-simrig.partial");
        let text = |path: &std::path::Path| path.to_string_lossy().into_owned();
        let mut compile: Vec<String> = [
            "--sdk",
            "iphonesimulator",
            "clang",
            "-target",
            target,
            "-DWONDER_SIMRIG_STANDALONE=1",
            "-fobjc-arc",
            "-Wall",
            "-Werror",
            "-O1",
            "-framework",
            "Foundation",
            "-framework",
            "CoreFoundation",
        ]
        .map(str::to_owned)
        .to_vec();
        compile.extend([
            text(&source),
            "-o".to_owned(),
            text(&partial),
            format!(
                "-Wl,-sectcreate,__TEXT,__entitlements,{}",
                text(&entitlements)
            ),
        ]);
        if !self.run(XCRUN, compile, None, BOOT_TIMEOUT).await?.success {
            return Err(SimError::Failed(FAILED));
        }
        let sign = ["-s", "-", "--force"]
            .map(str::to_owned)
            .into_iter()
            .chain([text(&partial)])
            .collect();
        if !self.run(CODESIGN, sign, None, CALL_TIMEOUT).await?.success {
            return Err(SimError::Failed(FAILED));
        }
        tokio::fs::rename(&partial, &binary).await.map_err(io)?;
        Ok((binary, digest))
    }

    /// Runs the helper inside the simulator, from its own `tmp`.
    async fn rig(&self, udid: &str, args: &[&str]) -> Result<(), SimError> {
        const FAILED: &str = "The simulator's hinge couldn't be moved. Try again.";
        let (binary, digest) = self.rig_binary().await?;
        let home = self.simctl(&["getenv", udid, "HOME"]).await?;
        let home = PathBuf::from(String::from_utf8_lossy(&home.stdout).trim());
        // Only into this simulator's own data folder, whatever getenv says.
        let expected = self
            .devices_root
            .as_ref()
            .map(|root| root.join(udid).join("data"));
        if expected.as_ref() != Some(&home) {
            return Err(SimError::Failed(FAILED));
        }
        let destination = home.join("tmp").join(format!("wonder-simrig-{digest}"));
        if tokio::fs::metadata(&destination).await.is_err() {
            tokio::fs::create_dir_all(home.join("tmp"))
                .await
                .map_err(|_| SimError::Failed(FAILED))?;
            // Copy beside it and rename, so an interrupted copy never leaves a
            // truncated helper that every later call would run.
            let partial = destination.with_extension("partial");
            let copied = tokio::fs::copy(&binary, &partial).await;
            if copied.is_err() || tokio::fs::rename(&partial, &destination).await.is_err() {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(SimError::Failed(FAILED));
            }
        }
        let destination = destination.to_string_lossy().into_owned();
        let mut spawn = vec![
            "simctl".to_owned(),
            "spawn".to_owned(),
            udid.to_owned(),
            destination,
        ];
        spawn.extend(args.iter().map(|arg| (*arg).to_owned()));
        if self.run(XCRUN, spawn, None, RIG_TIMEOUT).await?.success {
            Ok(())
        } else {
            Err(SimError::Failed(FAILED))
        }
    }

    pub(crate) async fn screenshot(&self, udid: &str) -> Result<Screenshot, SimError> {
        let udid = self.booted(udid).await?;
        let failed = SimError::Failed("The simulator's screen couldn't be captured. Try again.");
        // A private directory per capture; dropping it removes the file.
        let directory = tempfile::Builder::new()
            .prefix("cupola-simulator-")
            .tempdir()
            .map_err(|_| failed.clone())?;
        let path = directory.path().join("screen.png");
        let path_text = path.to_str().ok_or(failed.clone())?.to_owned();
        let ran = self
            .simctl(&["io", &udid, "screenshot", "--type=png", &path_text])
            .await?;
        if !ran.success {
            return Err(failed);
        }
        let size = tokio::fs::metadata(&path)
            .await
            .map_err(|_| failed.clone())?
            .len();
        if size > MAX_SCREENSHOT_BYTES {
            return Err(SimError::Failed("The screenshot was too large to send."));
        }
        let png = tokio::fs::read(&path).await.map_err(|_| failed.clone())?;
        drop(directory);
        let (width, height) = png_dimensions(&png).ok_or(failed)?;
        let sha256 = hex::encode(Sha256::digest(&png));
        Ok(Screenshot {
            png,
            width,
            height,
            sha256,
        })
    }
}

/// Validates and canonicalizes (uppercase) a simulator UDID.
pub(crate) fn valid_udid(udid: &str) -> Option<String> {
    let groups: Vec<_> = udid.split('-').collect();
    let shaped = groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()));
    shaped.then(|| udid.to_ascii_uppercase())
}

pub(crate) fn valid_bundle_id(id: &str) -> bool {
    id.len() <= 155
        && id.split('.').count() >= 2
        && id.split('.').all(|part| {
            part.bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// A BCP-47 language with optional script and region: en, pt-BR, zh-Hans-CN.
/// Returns the matching AppleLocale (language_REGION, or the language alone).
pub(crate) fn locale_for(tag: &str) -> Option<String> {
    let parts: Vec<_> = tag.split('-').collect();
    let language = parts.first()?;
    if !(2..=3).contains(&language.len()) || !language.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let mut rest = &parts[1..];
    if let Some(script) = rest.first() {
        let bytes = script.as_bytes();
        if bytes.len() == 4
            && bytes[0].is_ascii_uppercase()
            && bytes[1..].iter().all(u8::is_ascii_lowercase)
        {
            rest = &rest[1..];
        }
    }
    match rest {
        [] => Some((*language).to_owned()),
        [region]
            if (region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()))
                || (region.len() == 3 && region.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Some(format!("{language}_{region}"))
        }
        _ => None,
    }
}

fn valid_locale(locale: &str) -> bool {
    locale.len() <= 32
        && !locale.is_empty()
        && locale
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'@' || b == b'=')
}

/// The first language of `defaults read -g AppleLanguages`, an old-style
/// plist array such as `(\n    "en-US",\n    fr\n)`.
pub(crate) fn first_language(output: &str) -> Option<String> {
    let inner = output.trim().strip_prefix('(')?.strip_suffix(')')?;
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('"'))
        .find(|item| !item.is_empty())
        .filter(|item| locale_for(item).is_some())
        .map(str::to_owned)
}

fn ui_value(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout).trim().to_owned()
}

/// simctl's answer, or `unknown` for anything else it might print.
pub(crate) fn appearance_value(value: &str) -> &'static str {
    ["light", "dark", "unsupported"]
        .into_iter()
        .find(|known| *known == value)
        .unwrap_or("unknown")
}

pub(crate) fn content_size_value(value: &str) -> &'static str {
    CONTENT_SIZES
        .into_iter()
        .chain(["unsupported"])
        .find(|known| *known == value)
        .unwrap_or("unknown")
}

/// Web links, and app links whose scheme is plain letters; nothing that reads
/// files or runs script, no control characters, at most 2 KB.
pub(crate) fn valid_url(raw: &str) -> Option<String> {
    if raw.is_empty()
        || raw.len() > MAX_URL_BYTES
        || raw.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                // Invisible and direction-changing characters disguise a link.
                || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
        })
    {
        return None;
    }
    let (scheme, _) = raw.split_once(':')?;
    let scheme_ok = scheme.len() <= 64
        && scheme
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic())
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+.-".contains(&b));
    let lower = scheme.to_ascii_lowercase();
    if !scheme_ok || REFUSED_SCHEMES.contains(&lower.as_str()) {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    let web = matches!(url.scheme(), "http" | "https");
    // `http:host` would parse as `http://host/`; ask for the full form.
    if web && (!raw[scheme.len()..].starts_with("://") || url.host_str().is_none_or(str::is_empty))
    {
        return None;
    }
    let canonical = url.as_str().to_owned();
    (canonical.len() <= MAX_URL_BYTES).then_some(canonical)
}

/// `expires` is wonderctl's local, timezone-less ISO time.
pub(crate) fn lease_active(text: &str, now: chrono::NaiveDateTime) -> bool {
    let Ok(lease) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    let Some(expires) = lease["expires"].as_str() else {
        return false;
    };
    let expires = chrono::NaiveDateTime::parse_from_str(expires, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(expires, "%Y-%m-%dT%H:%M:%S%.f"))
        .ok()
        .or_else(|| {
            chrono::DateTime::parse_from_rfc3339(expires)
                .ok()
                .map(|at| at.with_timezone(&chrono::Local).naive_local())
        });
    expires.is_some_and(|expires| expires > now)
}

/// Cupola's own test devices: the nightly's always, the rest under a lease.
pub(crate) fn reserved_for_tests(name: &str, lease_active: bool) -> bool {
    name.starts_with(TEST_DEVICE_PREFIX) && (lease_active || name.ends_with(NIGHTLY_DEVICE_SUFFIX))
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-27-1` → `iOS 27.1`.
fn runtime_from_identifier(identifier: &str) -> String {
    let tail = identifier.rsplit('.').next().unwrap_or(identifier);
    match tail.split_once('-') {
        Some((platform, version)) => format!("{platform} {}", version.replace('-', ".")),
        None => tail.to_owned(),
    }
}

fn wire_state(state: &str) -> &'static str {
    match state {
        "Booted" => "booted",
        "Shutdown" => "shutdown",
        "Booting" => "booting",
        "Shutting Down" => "shuttingDown",
        _ => "unknown",
    }
}

/// `simctl list --json`: available devices with runtime and type names.
pub(crate) fn parse_devices(listing: &Value, lease_active: bool) -> Vec<Device> {
    let names = |key: &str| -> std::collections::HashMap<String, String> {
        listing[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some((
                    item["identifier"].as_str()?.to_owned(),
                    item["name"].as_str()?.to_owned(),
                ))
            })
            .collect()
    };
    let runtimes = names("runtimes");
    let types = names("devicetypes");
    let mut devices: Vec<Device> = listing["devices"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(runtime, devices)| {
            let runtime_name = runtimes
                .get(runtime)
                .cloned()
                .unwrap_or_else(|| runtime_from_identifier(runtime));
            let types = &types;
            devices
                .as_array()
                .into_iter()
                .flatten()
                .filter(|device| device["isAvailable"].as_bool() != Some(false))
                .filter_map(move |device| {
                    let udid = valid_udid(device["udid"].as_str()?)?;
                    let name: String = device["name"].as_str()?.chars().take(160).collect();
                    let type_id = device["deviceTypeIdentifier"].as_str().unwrap_or("");
                    let state = wire_state(device["state"].as_str().unwrap_or(""));
                    Some(Device {
                        // A stopped test simulator can't be started from here.
                        in_use_by_tests: reserved_for_tests(&name, lease_active)
                            || (name.starts_with(TEST_DEVICE_PREFIX) && state != "booted"),
                        udid,
                        name,
                        runtime: runtime_name.clone(),
                        state,
                        device_type: types
                            .get(type_id)
                            .cloned()
                            .unwrap_or_else(|| runtime_from_identifier(type_id).replace(' ', "")),
                    })
                })
        })
        .collect();
    devices.sort_by(|a, b| {
        (a.state != "booted", &a.name, &a.runtime).cmp(&(b.state != "booted", &b.name, &b.runtime))
    });
    devices.truncate(MAX_LISTED);
    devices
}

/// Owner-installed apps from `simctl listapps` (as JSON), by name.
pub(crate) fn user_apps(apps: &Value) -> Vec<Value> {
    let mut list: Vec<(String, String)> = apps
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, app)| app["ApplicationType"] == "User")
        .filter(|(id, _)| valid_bundle_id(id))
        .map(|(id, app)| {
            let name = app["CFBundleDisplayName"]
                .as_str()
                .or(app["CFBundleName"].as_str())
                .unwrap_or(id);
            (name.chars().take(160).collect(), id.clone())
        })
        .collect();
    list.sort();
    list.truncate(MAX_APPS);
    list.into_iter()
        .map(|(name, id)| json!({"bundleId": id, "name": name}))
        .collect()
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !bytes.starts_with(SIGNATURE) {
        return None;
    }
    image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png)
        .into_dimensions()
        .ok()
        .filter(|(width, height)| *width > 0 && *height > 0)
}

/// The host the phone is talking to, so it can label simulators across
/// paired computers.
pub(crate) async fn host(state: &AppState) -> Value {
    json!({
        "id": state.host_installation_id,
        "name": crate::host_name::current(state).await,
    })
}

pub(crate) async fn list_json(state: &AppState) -> Value {
    let host = host(state).await;
    match state.simulators.list().await {
        Ok(devices) => json!({
            "available": true,
            "host": host,
            "simulators": devices.iter().map(Device::json).collect::<Vec<_>>(),
        }),
        Err(error) => json!({
            "available": false,
            "reason": error.message(),
            "host": host,
            "simulators": [],
        }),
    }
}

// ----- Phone routes -----

#[allow(clippy::result_large_err)]
fn paired(authenticated: Option<Extension<AuthenticatedDevice>>) -> Result<(), Response> {
    authenticated
        .map(|_| ())
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "paired device required").into_response())
}

fn reply(result: Result<Value, SimError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(crate) async fn list_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    Json(list_json(&state).await).into_response()
}

pub(crate) async fn settings_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(state.simulators.settings(&udid).await)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AppearanceRequest {
    appearance: String,
}

pub(crate) async fn appearance_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<AppearanceRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(
        state
            .simulators
            .set_appearance(&udid, &request.appearance)
            .await,
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContentSizeRequest {
    content_size: String,
}

pub(crate) async fn content_size_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<ContentSizeRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(
        state
            .simulators
            .set_content_size(&udid, &request.content_size)
            .await,
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LanguageRequest {
    language: String,
}

pub(crate) async fn language_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<LanguageRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(
        state
            .simulators
            .set_language(&udid, &request.language)
            .await,
    )
}

pub(crate) async fn apps_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(state.simulators.apps(&udid).await)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ResetPermissionsRequest {
    bundle_id: String,
}

pub(crate) async fn reset_permissions_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<ResetPermissionsRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(
        state
            .simulators
            .reset_permissions(&udid, &request.bundle_id)
            .await,
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OpenUrlRequest {
    url: String,
}

pub(crate) async fn open_url_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<OpenUrlRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(state.simulators.open_url(&udid, &request.url).await)
}

pub(crate) async fn boot_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(state.simulators.boot(&udid).await)
}

pub(crate) async fn shutdown_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(state.simulators.shutdown(&udid).await)
}

#[derive(Deserialize)]
pub(crate) struct HingeRequest {
    degrees: u16,
}

pub(crate) async fn hinge_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<HingeRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(state.simulators.set_hinge(&udid, request.degrees).await)
}

#[derive(Deserialize)]
pub(crate) struct OrientationRequest {
    orientation: String,
}

pub(crate) async fn orientation_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
    Json(request): Json<OrientationRequest>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(
        state
            .simulators
            .set_orientation(&udid, &request.orientation)
            .await,
    )
}

/// Base64 in JSON like app icons, so the phone's signed JSON client reads it.
pub(crate) async fn screenshot_route(
    State(state): State<AppState>,
    authenticated: Option<Extension<AuthenticatedDevice>>,
    Path(udid): Path<String>,
) -> Response {
    if let Err(response) = paired(authenticated) {
        return response;
    }
    reply(
        state
            .simulators
            .screenshot(&udid)
            .await
            .map(|shot| shot.json(&udid.to_ascii_uppercase())),
    )
}

// The tests use test_support, which is unix-only.
#[cfg(all(test, unix))]
#[path = "simulators_tests.rs"]
pub(crate) mod tests;
