//! Cupola's skill pack on the wire, against the fake Codex runtime and the
//! fake Claude bridge: what each receives at session start and per turn with
//! every skill off (byte for byte what main sent, recorded in
//! `wonder_skills_off_baseline.json`) and with the skills on.
use crate::provider_switch_tests::{fx, Fx};
use serde_json::{json, Value};
use wonder_store::{AgentFamily, MessageInsert, StoredMessage};

const PHONE: &str = "owner";
/// A new Project thread, so Codex's session start is recorded too.
const CHAT: &str = "00000000-0000-4000-8000-0000000000aa";
const DESKTOP: &str = "wonder-desktop";
/// Session start and turn methods, plus the skill root call.
const SENT: [&str; 4] = [
    "thread/start",
    "thread/resume",
    "turn/start",
    "skills/extraRoots/set",
];

/// Accepts a message from `device` the way the send route does. Client ids
/// are fixed so recorded requests compare exactly.
async fn queue_from(
    fx: &Fx,
    device: &str,
    client: &str,
    text: &str,
    with: Option<(AgentFamily, &str)>,
) -> StoredMessage {
    if let Some((family, model)) = with {
        fx.state
            .store
            .stage_project_message_target(
                device,
                client,
                family,
                model,
                None,
                None,
                "2026-01-01T00:00:00Z",
            )
            .await
            .unwrap();
    }
    let MessageInsert::Inserted(message) = fx
        .state
        .store
        .insert_dispatch_message(
            device,
            client,
            text,
            "hash",
            CHAT,
            &[],
            "2026-01-01T00:00:00Z",
            true,
        )
        .await
        .unwrap()
    else {
        panic!("new message")
    };
    message
}

/// A new Codex thread in the fixture's Project, not yet started.
async fn new_thread(fx: &Fx) {
    fx.state.store.ensure_local_desktop("now").await.unwrap();
    let project = fx.state.store.project("project").await.unwrap().unwrap();
    fx.state
        .store
        .create_project_conversation(wonder_store::ProjectConversationInsert {
            conversation_id: CHAT,
            project_id: "project",
            family: AgentFamily::Codex,
            provider_store: &fx.state.projects.codex_store,
            native_session_id: None,
            cwd: &project.roots[0].path,
            roots_revision: project.roots_revision,
            title: "Skills",
            model: Some("fake"),
            effort: None,
            service_tier: None,
            access_mode: "workspace",
            claude_approval: "ask",
            plan_mode: false,
            creation_request_id: None,
            now: "now",
        })
        .await
        .unwrap();
    std::fs::write(fx.dir.path().join("next-thread"), "fresh-codex").unwrap();
}

/// The same four turns for every check: the phone starts a Codex thread and
/// this Mac continues it, then the phone moves it to Claude and this Mac
/// follows.
pub(crate) async fn converse(fx: &Fx) {
    new_thread(fx).await;
    let turns = [
        (
            PHONE,
            "00000000-0000-4000-8000-000000000001",
            "from the phone",
            None,
        ),
        (
            DESKTOP,
            "00000000-0000-4000-8000-000000000002",
            "from the mac",
            None,
        ),
        (
            PHONE,
            "00000000-0000-4000-8000-000000000003",
            "phone moves to claude",
            Some((AgentFamily::Claude, "claude:sonnet")),
        ),
        (
            DESKTOP,
            "00000000-0000-4000-8000-000000000004",
            "mac continues in claude",
            None,
        ),
    ];
    for (device, client, text, with) in turns {
        let message = queue_from(fx, device, client, text, with).await;
        fx.release(&message).await.unwrap();
        fx.finish(&message).await;
    }
}

/// What one provider received, in order, with this fixture's temporary
/// paths and random conversation id replaced by stable names.
fn sent(fx: &Fx, log: &str) -> Vec<Value> {
    let dir = fx.dir.path().to_string_lossy().into_owned();
    let canonical = crate::canonical_path::canonicalize(fx.dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::read_to_string(fx.dir.path().join(log))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|request| SENT.iter().any(|method| request["method"] == *method))
        .map(|request| {
            let text = json!({"method": request["method"], "params": request["params"]})
                .to_string()
                .replace(&canonical, "<fixture>")
                .replace(&dir, "<fixture>")
                .replace(&fx.chat, "<conversation>");
            let mut request: Value = serde_json::from_str(&text).unwrap();
            // A root is sent as given and canonical; both become <fixture>
            // where the temp dir is a symlink (macOS /var) but not on Linux.
            if let Some(Value::Array(roots)) =
                request.pointer_mut("/params/wonderPolicy/writeRoots")
            {
                roots.dedup();
            }
            request
        })
        .collect()
}

pub(crate) fn recorded(fx: &Fx) -> Value {
    json!({"codex": sent(fx, "requests-jsonl"), "claude": sent(fx, "claude-requests")})
}

// Contract: with every Cupola skill off, Cupola sends Codex and Claude
// exactly what it sent before the skill pack existed, at session start and
// on every turn, from the phone or the Mac. The baseline was recorded on main
// (5a496dbf) with this scenario; set WONDER_RECORD_SKILLS_BASELINE=1 to
// record it again only for a deliberate wire change.
#[tokio::test]
async fn skills_off_send_exactly_what_main_sent() {
    let fx = fx().await;
    converse(&fx).await;
    let actual = recorded(&fx);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/wonder_skills_off_baseline.json");
    if std::env::var_os("WONDER_RECORD_SKILLS_BASELINE").is_some() {
        std::fs::write(&path, serde_json::to_string_pretty(&actual).unwrap() + "\n").unwrap();
        return;
    }
    let baseline: Value =
        serde_json::from_str(include_str!("wonder_skills_off_baseline.json")).unwrap();
    assert_eq!(
        actual["codex"].as_array().map(Vec::len),
        Some(4),
        "Codex: start, turn, resume, turn: {actual}"
    );
    assert_eq!(actual, baseline);
}

// Contract: with the skills on, each session can discover exactly the
// enabled skills from Cupola's own folder (Codex: the runtime's extra skill
// root, set once per runtime; Claude: a per-process local plugin), only
// turns the owner wrote on the phone carry the phone signal, and nothing
// else in what is sent changes.
#[tokio::test]
async fn skills_on_reach_wonder_sessions_and_only_phone_turns_carry_the_signal() {
    use crate::wonder_skills::{PHONE_CONTEXT, SKILLS};
    let baseline: Value =
        serde_json::from_str(include_str!("wonder_skills_off_baseline.json")).unwrap();
    let fx = fx().await;
    for skill in &SKILLS {
        fx.state
            .store
            .set_wonder_skill(skill.id, true, "now")
            .await
            .unwrap();
    }
    converse(&fx).await;
    let actual = recorded(&fx);
    let codex = actual["codex"].as_array().unwrap();
    let claude = actual["claude"].as_array().unwrap();

    // The folder holds the shipped skills, nowhere near ~/.codex or ~/.claude.
    let folder = fx.dir.path().join("agent-skills");
    for skill in &SKILLS {
        let path = folder.join("skills").join(skill.id).join("SKILL.md");
        assert_eq!(std::fs::read_to_string(path).unwrap(), skill.body);
    }
    // Codex: the root is set before the session opens, once for this runtime.
    assert_eq!(codex[0]["method"], "skills/extraRoots/set");
    assert_eq!(
        codex[0]["params"],
        json!({"extraRoots": ["<fixture>/agent-skills/skills"]})
    );
    assert_eq!(
        codex
            .iter()
            .filter(|r| r["method"] == "skills/extraRoots/set")
            .count(),
        1
    );
    // Everything else Codex receives is unchanged, except the phone turn's
    // native turn context.
    let mut rest: Vec<Value> = codex[1..].to_vec();
    let preamble = json!({"kind": "application", "value": crate::preamble::text(true)});
    assert_eq!(
        rest[1]["params"]["additionalContext"],
        json!({"wonder_phone": {"kind": "application", "value": PHONE_CONTEXT},
            crate::preamble::CONTEXT_KEY: preamble})
    );
    assert_eq!(
        rest[3]["params"]["additionalContext"],
        json!({crate::preamble::CONTEXT_KEY: preamble}),
        "Mac turn"
    );
    rest[1]["params"]["additionalContext"]
        .as_object_mut()
        .unwrap()
        .remove("wonder_phone");
    assert_eq!(Value::Array(rest), baseline["codex"]);

    // Claude: session start is unchanged; every turn names the plugin and its
    // enabled skills; only the phone turn carries the phone line, as turn
    // context beside the owner's message, whose input is exactly their words.
    let starts: Vec<&Value> = claude
        .iter()
        .filter(|r| r["method"] == "turn/start")
        .collect();
    assert_eq!(starts.len(), 2);
    let pack = json!({"plugin": "<fixture>/agent-skills",
        "skills": SKILLS.iter().map(|skill| format!("wonder:{}", skill.id)).collect::<Vec<_>>()});
    let last_text = |r: &Value| {
        r["params"]["input"].as_array().unwrap().last().unwrap()["text"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(starts[0]["params"]["wonderSkills"], pack);
    assert_eq!(starts[0]["params"]["wonderContext"], json!([PHONE_CONTEXT]));
    // After the provider switch the input leads with the earlier conversation;
    // the owner's words follow, and nothing else.
    let input = starts[0]["params"]["input"].as_array().unwrap();
    assert!(crate::provider_switch::is_handoff_text(
        input[0]["text"].as_str().unwrap()
    ));
    assert_eq!(
        input[1..],
        [json!({"type": "text", "text": "phone moves to claude"})]
    );
    assert_eq!(starts[1]["params"]["wonderSkills"], pack);
    assert!(
        starts[1]["params"].get("wonderContext").is_none(),
        "Mac turn"
    );
    assert_eq!(last_text(starts[1]), "mac continues in claude");
    let mut stripped: Vec<Value> = claude.clone();
    for request in &mut stripped {
        if request["method"] == "turn/start" {
            let params = request["params"].as_object_mut().unwrap();
            params.remove("wonderSkills");
            params.remove("wonderContext");
        }
    }
    assert_eq!(Value::Array(stripped), baseline["claude"]);

    // Earlier turns carried the phone line in their input; the history views
    // never show it as the owner's words.
    let item = crate::history::sanitize_typed_item(
        &json!({
            "type": "userMessage", "id": "u1",
            "content": [{"type": "text", "text": "phone moves to claude"}, {"type": "text", "text": PHONE_CONTEXT}]
        }),
        None,
    );
    assert_eq!(
        item["content"],
        json!([{"type": "text", "text": "phone moves to claude"}])
    );
}

// Contract: once every skill is off again, the runtime that had the root is
// cleared once, and turns carry nothing.
#[tokio::test]
async fn turning_skills_off_clears_the_runtime_root_once() {
    use crate::wonder_skills::SKILLS;
    let fx = fx().await;
    new_thread(&fx).await;
    fx.state
        .store
        .set_wonder_skill(SKILLS[0].id, true, "now")
        .await
        .unwrap();
    let first = queue_from(
        &fx,
        PHONE,
        "00000000-0000-4000-8000-000000000011",
        "one",
        None,
    )
    .await;
    fx.release(&first).await.unwrap();
    fx.finish(&first).await;
    fx.state
        .store
        .set_wonder_skill(SKILLS[0].id, false, "now")
        .await
        .unwrap();
    for client in [
        "00000000-0000-4000-8000-000000000012",
        "00000000-0000-4000-8000-000000000013",
    ] {
        let message = queue_from(&fx, PHONE, client, "more", None).await;
        fx.release(&message).await.unwrap();
        fx.finish(&message).await;
    }
    let codex = sent(&fx, "requests-jsonl");
    let roots: Vec<&Value> = codex
        .iter()
        .filter(|r| r["method"] == "skills/extraRoots/set")
        .map(|r| &r["params"]["extraRoots"])
        .collect();
    assert_eq!(
        roots,
        [&json!(["<fixture>/agent-skills/skills"]), &json!([])]
    );
    let turns: Vec<&Value> = codex
        .iter()
        .filter(|r| r["method"] == "turn/start")
        .collect();
    assert!(turns[0]["params"]["additionalContext"]
        .get("wonder_phone")
        .is_some());
    assert!(turns[1..].iter().all(|r| r["params"]["additionalContext"]
        .get("wonder_phone")
        .is_none()));
}
