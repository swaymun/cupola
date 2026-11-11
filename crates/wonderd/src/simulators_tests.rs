//! Contracts for typed simulator actions: arguments are validated before any
//! process runs, each action is one exact simctl vector (no shell), answers
//! are read back, and Cupola's test simulators are refused under a lease.
use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

pub(crate) const BOOTED: &str = "A1B2C3D4-0000-4000-8000-000000000001";
const SHUTDOWN: &str = "A1B2C3D4-0000-4000-8000-000000000002";
pub(crate) const TEST_DEVICE: &str = "A1B2C3D4-0000-4000-8000-000000000003";
const NIGHTLY: &str = "A1B2C3D4-0000-4000-8000-000000000004";
const GONE: &str = "A1B2C3D4-0000-4000-8000-000000000005";
const DEMO_DUO: &str = "A1B2C3D4-0000-4000-8000-000000000006";

fn listing() -> Value {
    json!({
        "devicetypes": [
            {"identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro", "name": "iPhone 17 Pro"}
        ],
        "runtimes": [
            {"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-26-5", "name": "iOS 26.5"}
        ],
        "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
                {"udid": SHUTDOWN, "name": "iPhone 17 Pro", "state": "Shutdown", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"},
                {"udid": BOOTED.to_lowercase(), "name": "Demo iPhone", "state": "Booted", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"},
                {"udid": GONE, "name": "Old", "state": "Shutdown", "isAvailable": false,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-27-1": [
                {"udid": TEST_DEVICE, "name": "Wonder Control iPhone Duo", "state": "Booted", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-Duo"},
                {"udid": NIGHTLY, "name": "Wonder Control iPhone Nightly", "state": "Booted", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"},
                {"udid": DEMO_DUO, "name": "Demo iPhone Duo", "state": "Booted", "isAvailable": true,
                 "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-Duo"}
            ]
        }
    })
}

pub(crate) fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::new(width, height)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// Records every invocation and answers like simctl, keeping written settings.
#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) calls: Mutex<Vec<Invocation>>,
    pub(crate) settings: Mutex<HashMap<String, String>>,
    pub(crate) screenshot: Mutex<Option<Vec<u8>>>,
    pub(crate) screenshot_path: Mutex<Option<PathBuf>>,
    pub(crate) timed_out: bool,
    /// Stands in for CoreSimulator's device-data folder.
    pub(crate) devices_root: PathBuf,
}

fn ok(stdout: &str) -> Result<Ran, RunFailure> {
    Ok(Ran {
        success: true,
        stdout: stdout.as_bytes().to_vec(),
    })
}

fn failed() -> Result<Ran, RunFailure> {
    Ok(Ran::default())
}

impl Fake {
    fn answer(&self, invocation: &Invocation) -> Result<Ran, RunFailure> {
        if invocation.program == PLUTIL {
            return ok(&json!({
                "com.example.notes": {"ApplicationType": "User", "CFBundleDisplayName": "Notes Pro"},
                "com.example.zeta": {"ApplicationType": "User", "CFBundleName": "Zeta"},
                "com.apple.mobilesafari": {"ApplicationType": "System", "CFBundleDisplayName": "Safari"}
            })
            .to_string());
        }
        if invocation.program == CODESIGN {
            return ok("");
        }
        if invocation.args.first().map(String::as_str) == Some("--sdk") {
            // The clang build: leave an executable where `-o` says.
            let at = invocation.args.iter().position(|arg| arg == "-o").unwrap();
            std::fs::write(&invocation.args[at + 1], b"rig").unwrap();
            return ok("");
        }
        if self.timed_out {
            return Err(RunFailure::TimedOut);
        }
        let args: Vec<&str> = invocation.args.iter().map(String::as_str).collect();
        let mut settings = self.settings.lock().unwrap();
        let read = |settings: &HashMap<String, String>, key: &str, default: &str| {
            settings
                .get(key)
                .cloned()
                .unwrap_or_else(|| default.to_owned())
        };
        match args.as_slice() {
            ["simctl", "list", "--json"] => ok(&listing().to_string()),
            ["simctl", "ui", _, option] => ok(&format!(
                "{}\n",
                read(
                    &settings,
                    option,
                    if *option == "appearance" {
                        "light"
                    } else {
                        "large"
                    }
                )
            )),
            ["simctl", "ui", _, option, value] => {
                settings.insert((*option).into(), (*value).into());
                ok("")
            }
            ["simctl", "spawn", _, "defaults", "read", "-g", "AppleLanguages"] => ok(&format!(
                "(\n    \"{}\",\n    en\n)\n",
                read(&settings, "AppleLanguages", "en-US")
            )),
            ["simctl", "spawn", _, "defaults", "read", "-g", "AppleLocale"] => {
                ok(&format!("{}\n", read(&settings, "AppleLocale", "en_US")))
            }
            ["simctl", "spawn", _, "defaults", "write", "-g", key, _, value] => {
                settings.insert((*key).into(), (*value).into());
                ok("")
            }
            ["simctl", "get_app_container", _, "com.example.notes"] => ok("/apps/Notes.app\n"),
            ["simctl", "get_app_container", ..] => failed(),
            ["simctl", "privacy", ..] => ok(""),
            ["simctl", "listapps", _] => ok("{ }"),
            ["simctl", "openurl", _, url] if url.contains("nohandler") => failed(),
            ["simctl", "openurl", ..] => ok(""),
            ["simctl", "getenv", udid, "HOME"] => {
                ok(&format!("{}/{udid}/data\n", self.devices_root.display()))
            }
            ["simctl", "spawn", _, path, "hinge" | "orient", _]
                if path.contains("wonder-simrig-") =>
            {
                ok("wonder-simrig: dispatched")
            }
            ["simctl", "boot" | "shutdown", _] | ["simctl", "bootstatus", _, "-b"] => ok(""),
            ["simctl", "io", _, "screenshot", "--type=png", path] => {
                let path = PathBuf::from(path);
                if let Some(bytes) = self.screenshot.lock().unwrap().as_ref() {
                    std::fs::write(&path, bytes).unwrap();
                }
                *self.screenshot_path.lock().unwrap() = Some(path);
                ok("")
            }
            _ => panic!("unexpected invocation {args:?}"),
        }
    }
}

impl Runner for Fake {
    fn run(&self, invocation: Invocation) -> BoxFuture<'_, Result<Ran, RunFailure>> {
        let answer = self.answer(&invocation);
        self.calls.lock().unwrap().push(invocation);
        Box::pin(async move { answer })
    }
}

pub(crate) struct Rig {
    pub(crate) fake: Arc<Fake>,
    pub(crate) simulators: Simulators,
    pub(crate) lease: tempfile::TempDir,
}

impl Rig {
    pub(crate) fn new() -> Self {
        Self::with(Fake::default())
    }
    pub(crate) fn with(mut fake: Fake) -> Self {
        let lease = tempfile::tempdir().unwrap();
        fake.devices_root = lease.path().join("devices");
        let fake = Arc::new(fake);
        let simulators = Simulators::new(
            Some(fake.clone() as Arc<dyn Runner>),
            Some(lease.path().join("simulator-lease.json")),
        )
        .with_rig(
            Some(lease.path().join("simrig")),
            Some(lease.path().join("devices")),
        );
        Self {
            fake,
            simulators,
            lease,
        }
    }
    /// Another handle on the same fake and lease, for an AppState.
    pub(crate) fn shared(&self) -> Simulators {
        Simulators::new(
            Some(self.fake.clone() as Arc<dyn Runner>),
            Some(self.lease.path().join("simulator-lease.json")),
        )
        .with_rig(
            Some(self.lease.path().join("simrig")),
            Some(self.lease.path().join("devices")),
        )
    }
    pub(crate) fn hold_lease(&self, expires: chrono::NaiveDateTime) {
        std::fs::write(
            self.lease.path().join("simulator-lease.json"),
            json!({"holder": "/checkouts/wt-other", "note": "scenario",
                   "expires": expires.format("%Y-%m-%dT%H:%M:%S").to_string()})
            .to_string(),
        )
        .unwrap();
    }
    /// Argument vectors after the booted check's listing.
    pub(crate) fn actions(&self) -> Vec<Vec<String>> {
        self.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.args != ["simctl", "list", "--json"])
            .map(|call| call.args.clone())
            .collect()
    }
    fn programs(&self) -> Vec<&'static str> {
        self.fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.program)
            .collect()
    }
    fn calls(&self) -> usize {
        self.fake.calls.lock().unwrap().len()
    }
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

#[tokio::test]
async fn each_action_runs_one_exact_simctl_vector_and_reads_it_back() {
    let rig = Rig::new();
    let s = &rig.simulators;
    // Lowercase IDs are canonicalized before they reach simctl.
    let udid = BOOTED.to_lowercase();

    let reply = s.set_appearance(&udid, "dark").await.unwrap();
    assert_eq!(
        reply,
        json!({"udid": BOOTED, "appearance": "dark", "applied": true})
    );
    let reply = s
        .set_content_size(&udid, "accessibility-extra-large")
        .await
        .unwrap();
    assert_eq!(reply["contentSize"], "accessibility-extra-large");
    assert_eq!(reply["applied"], true);
    let reply = s.set_language(&udid, "zh-Hans-CN").await.unwrap();
    assert_eq!(reply["language"], "zh-Hans-CN");
    assert_eq!(reply["locale"], "zh_CN");
    assert_eq!(reply["applied"], true);
    assert_eq!(reply["needsRestart"], true);
    let reply = s
        .reset_permissions(&udid, "com.example.notes")
        .await
        .unwrap();
    assert_eq!(reply["reset"], true);
    let reply = s
        .open_url(&udid, "https://example.com/a?b=c")
        .await
        .unwrap();
    assert_eq!(reply["opened"], true);
    s.open_url(&udid, "myapp://path/x").await.unwrap();

    assert_eq!(
        rig.actions(),
        vec![
            argv(&["simctl", "ui", BOOTED, "appearance", "dark"]),
            argv(&["simctl", "ui", BOOTED, "appearance"]),
            argv(&[
                "simctl",
                "ui",
                BOOTED,
                "content_size",
                "accessibility-extra-large"
            ]),
            argv(&["simctl", "ui", BOOTED, "content_size"]),
            argv(&[
                "simctl",
                "spawn",
                BOOTED,
                "defaults",
                "write",
                "-g",
                "AppleLanguages",
                "-array",
                "zh-Hans-CN"
            ]),
            argv(&[
                "simctl",
                "spawn",
                BOOTED,
                "defaults",
                "write",
                "-g",
                "AppleLocale",
                "-string",
                "zh_CN"
            ]),
            argv(&[
                "simctl",
                "spawn",
                BOOTED,
                "defaults",
                "read",
                "-g",
                "AppleLanguages"
            ]),
            argv(&[
                "simctl",
                "spawn",
                BOOTED,
                "defaults",
                "read",
                "-g",
                "AppleLocale"
            ]),
            argv(&["simctl", "get_app_container", BOOTED, "com.example.notes"]),
            argv(&[
                "simctl",
                "privacy",
                BOOTED,
                "reset",
                "all",
                "com.example.notes"
            ]),
            argv(&["simctl", "openurl", BOOTED, "https://example.com/a?b=c"]),
            argv(&["simctl", "openurl", BOOTED, "myapp://path/x"]),
        ]
    );
    // Only xcrun runs, never a shell; no call carries stdin.
    assert!(rig.programs().iter().all(|program| *program == XCRUN));

    let settings = s.settings(BOOTED).await.unwrap();
    assert_eq!(
        settings,
        json!({"udid": BOOTED, "appearance": "dark", "contentSize": "accessibility-extra-large",
               "language": "zh-Hans-CN", "locale": "zh_CN"})
    );
}

#[tokio::test]
async fn app_list_converts_simctl_plist_through_plutil_stdin() {
    let rig = Rig::new();
    let reply = rig.simulators.apps(BOOTED).await.unwrap();
    assert_eq!(
        reply["apps"],
        json!([{"bundleId": "com.example.notes", "name": "Notes Pro"},
               {"bundleId": "com.example.zeta", "name": "Zeta"}])
    );
    let calls = rig.fake.calls.lock().unwrap();
    let plutil = calls.last().unwrap();
    assert_eq!(plutil.program, PLUTIL);
    assert_eq!(
        plutil.args,
        argv(&["-convert", "json", "-o", "-", "--", "-"])
    );
    assert_eq!(plutil.stdin.as_deref(), Some(b"{ }".as_slice()));
}

#[tokio::test]
async fn invalid_arguments_are_refused_before_any_process_runs() {
    let rig = Rig::new();
    let s = &rig.simulators;
    let long_url = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
    let refusals = [
        s.set_appearance("not-a-udid", "dark").await,
        s.set_appearance(&format!("{BOOTED};rm"), "dark").await,
        s.set_appearance(BOOTED, "sepia").await,
        s.set_appearance(BOOTED, "Dark").await,
        s.set_content_size(BOOTED, "huge").await,
        s.set_content_size(BOOTED, "increment").await,
        s.set_language(BOOTED, "english").await,
        s.set_language(BOOTED, "en_US").await,
        s.set_language(BOOTED, "-g").await,
        s.set_language(BOOTED, "en-US-x-private").await,
        s.reset_permissions(BOOTED, "notes").await,
        s.reset_permissions(BOOTED, "-com.example").await,
        s.reset_permissions(BOOTED, "com.example/../x").await,
        s.open_url(BOOTED, "file:///etc/passwd").await,
        s.open_url(BOOTED, "JavaScript:alert(1)").await,
        s.open_url(BOOTED, "data:text/html,<b>x</b>").await,
        s.open_url(BOOTED, "https://example.com/\u{0}").await,
        s.open_url(BOOTED, "https://exa mple.com").await,
        s.open_url(BOOTED, &long_url).await,
    ];
    for refusal in refusals {
        assert!(matches!(refusal, Err(SimError::Invalid(_))), "{refusal:?}");
    }
    assert_eq!(rig.calls(), 0);
}

#[test]
fn links_are_web_or_plain_app_schemes_only() {
    for allowed in [
        "https://example.com/a?b=c#d",
        "http://localhost:8080/",
        "myapp://path/x?y=1",
        "com.example.app:/callback",
        "mailto:someone@example.com",
    ] {
        assert!(valid_url(allowed).is_some(), "{allowed}");
    }
    for refused in [
        "",
        "example.com",
        "//example.com",
        "1app://x",
        "https://",
        "http:relative",
        "file:///etc/passwd",
        "FILE:///etc/passwd",
        "javascript:alert(1)",
        "data:,x",
        "blob:https://example.com/x",
        "about:blank",
        "vbscript:x",
        "view-source:https://example.com",
        "https://example.com/\nX",
        "https://example.com/\u{7f}",
        "https://example.com/\u{202e}",
        "my app://x",
    ] {
        assert!(valid_url(refused).is_none(), "{refused:?}");
    }
    assert!(valid_url(&format!("https://e.com/{}", "a".repeat(2048 - 14))).is_some());
    assert!(valid_url(&format!("https://e.com/{}", "a".repeat(2048 - 13))).is_none());
}

#[test]
fn simctl_answers_are_read_back_into_known_values() {
    assert_eq!(
        first_language("(\n    \"en-US\"\n)\n").as_deref(),
        Some("en-US")
    );
    assert_eq!(
        first_language("(\n    fr,\n    en\n)").as_deref(),
        Some("fr")
    );
    assert_eq!(first_language("()"), None);
    assert_eq!(first_language("en-US"), None);
    assert_eq!(first_language("(\n    \"not a tag\"\n)"), None);
    assert_eq!(appearance_value("dark"), "dark");
    assert_eq!(appearance_value("unsupported"), "unsupported");
    assert_eq!(appearance_value("Dark mode?"), "unknown");
    assert_eq!(content_size_value("extra-extra-large"), "extra-extra-large");
    assert_eq!(content_size_value("gigantic"), "unknown");
    assert_eq!(locale_for("en").as_deref(), Some("en"));
    assert_eq!(locale_for("pt-BR").as_deref(), Some("pt_BR"));
    assert_eq!(locale_for("es-419").as_deref(), Some("es_419"));
    assert_eq!(locale_for("sr-Latn").as_deref(), Some("sr"));
    assert_eq!(valid_udid(&BOOTED.to_lowercase()).as_deref(), Some(BOOTED));
    assert_eq!(valid_udid("A1B2C3D4-0000-4000-8000-00000000000G"), None);

    let devices = parse_devices(&listing(), false);
    let summary: Vec<_> = devices
        .iter()
        .map(|d| {
            (
                d.name.as_str(),
                d.runtime.as_str(),
                d.state,
                d.device_type.as_str(),
                d.in_use_by_tests,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("Demo iPhone", "iOS 26.5", "booted", "iPhone 17 Pro", false),
            ("Demo iPhone Duo", "iOS 27.1", "booted", "iPhoneDuo", false),
            // Names absent from the listing fall back to the identifiers.
            (
                "Wonder Control iPhone Duo",
                "iOS 27.1",
                "booted",
                "iPhoneDuo",
                false
            ),
            (
                "Wonder Control iPhone Nightly",
                "iOS 27.1",
                "booted",
                "iPhone 17 Pro",
                true
            ),
            (
                "iPhone 17 Pro",
                "iOS 26.5",
                "shutdown",
                "iPhone 17 Pro",
                false
            ),
        ]
    );
    assert_eq!(devices[0].udid, BOOTED);
}

#[test]
fn wonderctl_lease_marks_cupolas_test_simulators() {
    let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 25)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap();
    let lease = |expires: &str| json!({"holder": "/x", "expires": expires}).to_string();
    assert!(lease_active(&lease("2026-10-25T14:30:00"), now));
    assert!(!lease_active(&lease("2026-10-25T13:59:59"), now));
    assert!(!lease_active("not json", now));
    assert!(!lease_active("{}", now));
    assert!(reserved_for_tests("Wonder Control iPhone", true));
    assert!(!reserved_for_tests("Wonder Control iPhone", false));
    assert!(reserved_for_tests("Wonder Control iPad Nightly", false));
    assert!(!reserved_for_tests("My iPhone", true));
}

#[tokio::test]
async fn test_simulators_are_refused_while_leased_but_still_listed() {
    let rig = Rig::new();
    let later = chrono::Local::now().naive_local() + chrono::Duration::minutes(20);
    rig.hold_lease(later);
    let refused = rig.simulators.set_appearance(TEST_DEVICE, "dark").await;
    assert_eq!(refused, Err(SimError::InUseByTests));
    assert_eq!(
        refused.unwrap_err().message(),
        "This simulator is being used for tests right now."
    );
    assert!(rig.simulators.screenshot(TEST_DEVICE).await.is_err());
    // Nothing but the listing ran for the leased device.
    assert!(rig.actions().is_empty());
    let listed = rig.simulators.list().await.unwrap();
    assert!(listed
        .iter()
        .find(|d| d.udid == TEST_DEVICE)
        .is_some_and(|d| d.in_use_by_tests));
    rig.simulators.set_appearance(BOOTED, "dark").await.unwrap();

    // An expired lease frees it; the nightly's copies stay refused.
    rig.hold_lease(chrono::Local::now().naive_local() - chrono::Duration::minutes(1));
    rig.simulators
        .set_appearance(TEST_DEVICE, "dark")
        .await
        .unwrap();
    assert_eq!(
        rig.simulators
            .open_url(NIGHTLY, "https://example.com")
            .await,
        Err(SimError::InUseByTests)
    );
}

#[tokio::test]
async fn boot_starts_a_stopped_simulator_and_never_touches_test_devices() {
    let rig = Rig::new();
    let s = &rig.simulators;
    assert_eq!(
        s.boot(SHUTDOWN).await.unwrap(),
        json!({"udid": SHUTDOWN, "state": "booted"})
    );
    // Already running: no boot, just the wait, and still a success.
    s.boot(&BOOTED.to_lowercase()).await.unwrap();
    s.shutdown(BOOTED).await.unwrap();
    // Already stopped is a success without a call.
    assert_eq!(s.shutdown(SHUTDOWN).await.unwrap()["state"], "shutdown");
    assert_eq!(
        rig.actions(),
        vec![
            argv(&["simctl", "boot", SHUTDOWN]),
            argv(&["simctl", "bootstatus", SHUTDOWN, "-b"]),
            argv(&["simctl", "bootstatus", BOOTED, "-b"]),
            argv(&["simctl", "shutdown", BOOTED]),
        ]
    );
    // Test simulators are refused with no lease held, and unknown ones are not found.
    for udid in [TEST_DEVICE, NIGHTLY] {
        assert_eq!(s.boot(udid).await, Err(SimError::InUseByTests));
        assert_eq!(s.shutdown(udid).await, Err(SimError::InUseByTests));
    }
    assert_eq!(s.boot(GONE).await, Err(SimError::NotFound));
    assert_eq!(rig.actions().len(), 4);
}

#[tokio::test]
async fn only_booted_simulators_of_this_host_take_actions() {
    let rig = Rig::new();
    let s = &rig.simulators;
    assert_eq!(
        s.set_appearance(SHUTDOWN, "dark").await,
        Err(SimError::NotBooted)
    );
    assert_eq!(
        s.set_appearance(GONE, "dark").await,
        Err(SimError::NotFound)
    );
    assert_eq!(
        s.set_appearance("FFFFFFFF-0000-4000-8000-000000000000", "dark")
            .await,
        Err(SimError::NotFound)
    );
    assert_eq!(
        s.reset_permissions(BOOTED, "com.example.missing").await,
        Err(SimError::AppNotInstalled)
    );
    assert!(matches!(
        s.open_url(BOOTED, "myapp://nohandler").await,
        Err(SimError::Failed(_))
    ));
    assert!(rig.actions().iter().all(|args| args[1] != "privacy"));
    let disabled = Simulators::disabled();
    assert_eq!(
        disabled.list().await.unwrap_err().code(),
        "simulator-unavailable"
    );
    let timed_out = Rig::with(Fake {
        timed_out: true,
        ..Fake::default()
    });
    assert_eq!(
        timed_out.simulators.set_appearance(BOOTED, "dark").await,
        Err(SimError::Failed(TIMED_OUT))
    );
}

#[tokio::test]
async fn screenshots_are_validated_png_with_a_hash_and_leave_no_file() {
    let image = png(3, 2);
    let rig = Rig::with(Fake {
        screenshot: Mutex::new(Some(image.clone())),
        ..Fake::default()
    });
    let shot = rig.simulators.screenshot(BOOTED).await.unwrap();
    assert_eq!((shot.width, shot.height), (3, 2));
    assert_eq!(shot.sha256, hex::encode(Sha256::digest(&image)));
    let value = shot.json(BOOTED);
    assert_eq!(value["mimeType"], "image/png");
    assert_eq!(value["byteCount"], image.len());
    let path = rig.fake.screenshot_path.lock().unwrap().clone().unwrap();
    assert_eq!(path.file_name().unwrap(), "screen.png");
    assert!(!path.exists() && !path.parent().unwrap().exists());
    assert_eq!(
        rig.actions().last().unwrap()[..5],
        argv(&["simctl", "io", BOOTED, "screenshot", "--type=png"])[..]
    );

    for bytes in [
        b"GIF89a not a png".to_vec(),
        vec![0; MAX_SCREENSHOT_BYTES as usize + 1],
    ] {
        let rig = Rig::with(Fake {
            screenshot: Mutex::new(Some(bytes)),
            ..Fake::default()
        });
        assert!(matches!(
            rig.simulators.screenshot(BOOTED).await,
            Err(SimError::Failed(_))
        ));
        let path = rig.fake.screenshot_path.lock().unwrap().clone().unwrap();
        assert!(!path.exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn processes_pass_stdin_bound_output_and_report_missing_programs() {
    let ran = Processes
        .run(Invocation {
            program: "/bin/cat",
            args: vec![],
            stdin: Some(b"hello".to_vec()),
            timeout: CALL_TIMEOUT,
        })
        .await
        .unwrap();
    assert!(ran.success);
    assert_eq!(ran.stdout, b"hello");
    let flood = Processes
        .run(Invocation {
            program: "/usr/bin/yes",
            args: vec![],
            stdin: None,
            timeout: CALL_TIMEOUT,
        })
        .await;
    assert_eq!(flood.unwrap_err(), RunFailure::TooLarge);
    let missing = Processes
        .run(Invocation {
            program: "/nonexistent/simctl",
            args: vec![],
            stdin: None,
            timeout: CALL_TIMEOUT,
        })
        .await;
    assert_eq!(missing.unwrap_err(), RunFailure::Missing);
}

fn device() -> Option<Extension<AuthenticatedDevice>> {
    Some(Extension(AuthenticatedDevice {
        device_id: "phone".into(),
        session_binding: "fixture".into(),
    }))
}

async fn body(response: Response) -> (StatusCode, Option<String>, Value) {
    let status = response.status();
    let code = response
        .headers()
        .get(crate::projects::ERROR_CODE_HEADER)
        .map(|value| value.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let value =
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
    (status, code, value)
}

#[cfg(unix)]
#[tokio::test]
#[cfg(unix)] // Uses ingestion's Unix runtime and HTTP contract fixtures.
async fn phone_routes_need_a_paired_device_and_match_the_http_schema() {
    let (_dir, mut state) = crate::ingestion::tests::fixture().await;
    let rig = Rig::with(Fake {
        screenshot: Mutex::new(Some(png(4, 4))),
        ..Fake::default()
    });
    rig.hold_lease(chrono::Local::now().naive_local() + chrono::Duration::minutes(5));
    state.simulators = Arc::new(rig.shared());
    let st = || State(state.clone());
    let udid = || Path(BOOTED.to_owned());

    let (status, _, _) = body(list_route(st(), None).await).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = body(
        appearance_route(
            st(),
            None,
            udid(),
            Json(AppearanceRequest {
                appearance: "dark".into(),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(rig.calls(), 0);

    let (status, _, list) = body(list_route(st(), device()).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["host"]["id"], state.host_installation_id);
    assert_eq!(
        list["simulators"][0]["sourceId"],
        format!("simulator:{BOOTED}")
    );
    crate::test_support::assert_http_contract("simulatorList", &list);

    let replies = [
        (
            "simulatorSettings",
            settings_route(st(), device(), udid()).await,
        ),
        (
            "simulatorAppearanceResult",
            appearance_route(
                st(),
                device(),
                udid(),
                Json(AppearanceRequest {
                    appearance: "dark".into(),
                }),
            )
            .await,
        ),
        (
            "simulatorContentSizeResult",
            content_size_route(
                st(),
                device(),
                udid(),
                Json(ContentSizeRequest {
                    content_size: "small".into(),
                }),
            )
            .await,
        ),
        (
            "simulatorLanguageResult",
            language_route(
                st(),
                device(),
                udid(),
                Json(LanguageRequest {
                    language: "fr-FR".into(),
                }),
            )
            .await,
        ),
        ("simulatorApps", apps_route(st(), device(), udid()).await),
        (
            "simulatorPermissionsResetResult",
            reset_permissions_route(
                st(),
                device(),
                udid(),
                Json(ResetPermissionsRequest {
                    bundle_id: "com.example.notes".into(),
                }),
            )
            .await,
        ),
        (
            "simulatorOpenUrlResult",
            open_url_route(
                st(),
                device(),
                udid(),
                Json(OpenUrlRequest {
                    url: "https://example.com".into(),
                }),
            )
            .await,
        ),
        (
            "simulatorScreenshot",
            screenshot_route(st(), device(), udid()).await,
        ),
    ];
    for (definition, response) in replies {
        let (status, _, value) = body(response).await;
        assert_eq!(status, StatusCode::OK, "{definition}: {value}");
        crate::test_support::assert_http_contract(definition, &value);
    }

    let (status, code, message) =
        body(settings_route(st(), device(), Path(TEST_DEVICE.into())).await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code.as_deref(), Some("simulator-in-use-by-tests"));
    assert_eq!(message, "This simulator is being used for tests right now.");
    let (status, code, _) = body(
        open_url_route(
            st(),
            device(),
            udid(),
            Json(OpenUrlRequest {
                url: "file:///etc/hosts".into(),
            }),
        )
        .await,
    )
    .await;
    assert_eq!(
        (status, code.as_deref()),
        (StatusCode::BAD_REQUEST, Some("invalid"))
    );
}

/// Live, against a booted simulator this run owns. Run under the wonderctl
/// lease with WONDER_LIVE_SIMULATOR_UDID set (and WONDER_LIVE_EVIDENCE for the
/// screenshot); it changes appearance, text size and language, then puts them back.
#[tokio::test]
#[ignore = "needs a booted simulator"]
async fn live_simulator_actions() {
    let udid = std::env::var("WONDER_LIVE_SIMULATOR_UDID").expect("WONDER_LIVE_SIMULATOR_UDID");
    let s = Simulators::on_this_mac();
    let started = std::time::Instant::now();
    let listed = s.list().await.unwrap();
    println!(
        "list: {} simulators in {:?}",
        listed.len(),
        started.elapsed()
    );
    println!(
        "{}",
        json!(listed.iter().map(Device::json).collect::<Vec<_>>())
    );
    let before = s.settings(&udid).await.unwrap();
    println!("settings before: {before}");
    let evidence = std::env::var("WONDER_LIVE_EVIDENCE")
        .ok()
        .map(PathBuf::from);
    let capture = |name: &'static str| {
        let s = &s;
        let udid = &udid;
        let evidence = evidence.clone();
        async move {
            if let Some(dir) = evidence {
                let shot = s.screenshot(udid).await.unwrap();
                std::fs::write(dir.join(name), &shot.png).unwrap();
            }
        }
    };
    for (label, result) in [
        ("appearance dark", s.set_appearance(&udid, "dark").await),
        (
            "content size xxl",
            s.set_content_size(&udid, "extra-extra-large").await,
        ),
    ] {
        println!("{label}: {result:?}");
    }
    capture("01-dark-xxl.png").await;
    let opened = s.open_url(&udid, "https://example.com/").await;
    println!("open url: {opened:?}");
    tokio::time::sleep(Duration::from_secs(4)).await;
    capture("02-open-url.png").await;
    for (label, result) in [
        ("language fr-FR", s.set_language(&udid, "fr-FR").await),
        ("apps", s.apps(&udid).await),
        (
            "open bad scheme app link",
            s.open_url(&udid, "cupolanohandler://x").await,
        ),
        (
            "reset Safari permissions",
            s.reset_permissions(&udid, "com.apple.mobilesafari").await,
        ),
        (
            "reset missing app",
            s.reset_permissions(&udid, "com.example.not-installed")
                .await,
        ),
    ] {
        println!("{label}: {result:?}");
    }
    let shot = s.screenshot(&udid).await.unwrap();
    println!("screenshot: {}", shot.summary(&udid));
    if let Ok(dir) = std::env::var("WONDER_LIVE_EVIDENCE") {
        std::fs::write(PathBuf::from(dir).join("live-screenshot.png"), &shot.png).unwrap();
    }
    // Put the device back as it was.
    let restore_appearance = before["appearance"]
        .as_str()
        .filter(|v| APPEARANCES.contains(v));
    if let Some(appearance) = restore_appearance {
        println!("restore: {:?}", s.set_appearance(&udid, appearance).await);
    }
    if let Some(size) = before["contentSize"]
        .as_str()
        .filter(|v| CONTENT_SIZES.contains(v))
    {
        println!("restore: {:?}", s.set_content_size(&udid, size).await);
    }
    if let Some(language) = before["language"]
        .as_str()
        .filter(|_| std::env::var_os("WONDER_LIVE_KEEP_LANGUAGE").is_none())
    {
        println!("restore: {:?}", s.set_language(&udid, language).await);
    }
    println!("settings after: {:?}", s.settings(&udid).await);
}

/// Live: a booted "Wonder Control" simulator under an active wonderctl lease
/// is listed but refused (WONDER_LIVE_SIMULATOR_UDID names it).
#[tokio::test]
#[ignore = "needs a booted test simulator and an active wonderctl lease"]
async fn live_test_simulator_refused_under_lease() {
    let udid = std::env::var("WONDER_LIVE_SIMULATOR_UDID").expect("WONDER_LIVE_SIMULATOR_UDID");
    let s = Simulators::on_this_mac();
    let listed = s.list().await.unwrap();
    let device = listed.iter().find(|d| d.udid == udid).expect("listed");
    println!("listed: {}", device.json());
    assert!(device.in_use_by_tests);
    for result in [
        s.set_appearance(&udid, "dark").await.map(|_| ()),
        s.open_url(&udid, "https://example.com/").await.map(|_| ()),
        s.screenshot(&udid).await.map(|_| ()),
    ] {
        println!("refused: {result:?}");
        assert_eq!(result, Err(SimError::InUseByTests));
    }
}

#[tokio::test]
async fn hinge_and_orientation_run_the_rig_inside_a_booted_duo_only() {
    let rig = Rig::new();
    let s = &rig.simulators;

    // The helper is built once, signed, then copied into the Duo's own tmp.
    let reply = s.set_hinge(DEMO_DUO, 90).await.unwrap();
    assert_eq!(reply, json!({"udid": DEMO_DUO, "hinge": 90}));
    crate::test_support::assert_http_contract("simulatorHingeResult", &reply);
    let reply = s.set_orientation(DEMO_DUO, "pud").await.unwrap();
    assert_eq!(reply["orientation"], "pud");
    crate::test_support::assert_http_contract("simulatorOrientationResult", &reply);
    let actions = rig.actions();
    let compiles = actions.iter().filter(|args| args[0] == "--sdk").count();
    assert_eq!(compiles, 1, "built once for two actions");
    let spawns: Vec<_> = actions
        .iter()
        .filter(|args| args.get(1).map(String::as_str) == Some("spawn"))
        .collect();
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[0][2], DEMO_DUO);
    assert!(spawns[0][3].starts_with(
        rig.lease
            .path()
            .join("devices")
            .join(DEMO_DUO)
            .join("data/tmp/wonder-simrig-")
            .to_str()
            .unwrap()
    ));
    assert_eq!(spawns[0][4..], ["hinge", "90"]);
    assert_eq!(spawns[1][4..], ["orient", "pud"]);
    assert!(rig.programs().contains(&CODESIGN));

    // Arguments are checked before any process runs.
    let before = rig.calls();
    assert_eq!(
        s.set_hinge(DEMO_DUO, 181).await.unwrap_err().code(),
        "invalid"
    );
    assert_eq!(
        s.set_orientation(DEMO_DUO, "sideways")
            .await
            .unwrap_err()
            .code(),
        "invalid"
    );
    assert_eq!(rig.calls(), before);

    // A pooled test lane's posture belongs to its scenario, not to a phone.
    assert_eq!(
        s.set_hinge(TEST_DEVICE, 90).await.unwrap_err().code(),
        "invalid"
    );
    assert_eq!(
        s.set_orientation(TEST_DEVICE, "pud")
            .await
            .unwrap_err()
            .code(),
        "invalid"
    );

    // A phone has no hinge, and a stopped Duo can't be moved.
    assert_eq!(s.set_hinge(BOOTED, 90).await.unwrap_err().code(), "invalid");
    assert_eq!(
        s.set_hinge(SHUTDOWN, 90).await.unwrap_err().code(),
        "simulator-not-booted"
    );
}

#[tokio::test]
async fn the_device_list_says_which_simulators_have_a_hinge() {
    let rig = Rig::new();
    let devices = rig.simulators.list().await.unwrap();
    let hinge: Vec<_> = devices
        .iter()
        .filter(|device| device.json()["hasHinge"] == true)
        .map(|device| device.udid.as_str())
        .collect();
    assert_eq!(hinge, [DEMO_DUO, TEST_DEVICE]);
    // Test simulators say they can't be stopped; ordinary ones can.
    let stoppable: Vec<_> = devices
        .iter()
        .filter(|device| device.json()["canStop"] == true)
        .map(|device| device.udid.as_str())
        .collect();
    assert!(stoppable.contains(&BOOTED) && !stoppable.contains(&TEST_DEVICE));
}

/// Live check against a booted iPhone Duo simulator:
/// `WONDER_LIVE_DUO_UDID=<udid> cargo test -p wonderd live_duo_hinge -- --ignored`
#[tokio::test]
#[ignore = "needs a booted iPhone Duo simulator"]
async fn live_duo_hinge_runs_the_compiled_rig() {
    let Ok(udid) = std::env::var("WONDER_LIVE_DUO_UDID") else {
        panic!("set WONDER_LIVE_DUO_UDID to a booted iPhone Duo simulator");
    };
    let simulators = Simulators::on_this_mac();
    for degrees in [90, 0, 180] {
        let reply = simulators.set_hinge(&udid, degrees).await.unwrap();
        assert_eq!(reply["hinge"], degrees);
    }
    assert_eq!(
        simulators.set_orientation(&udid, "portrait").await.unwrap()["orientation"],
        "portrait"
    );
}
