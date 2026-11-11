//! The owner's per-provider additions on the wire, against the fake Codex
//! runtime and the fake Claude bridge, compared with what main sent before
//! they existed (`wonder_skills_off_baseline.json`, the same four turns):
//! with the defaults nothing changes (`skills_off_send_exactly_what_main_sent`
//! covers that), and each setting changes only its own field.
use crate::agent_settings::INSTRUCTION_CONTEXT_KEY;
use crate::preamble::CONTEXT_KEY as PREAMBLE_KEY;
use crate::provider_switch_tests::fx;
use crate::wonder_skills_tests::{converse, recorded};
use serde_json::{json, Value};
use wonder_store::AgentFamily;

fn baseline() -> Value {
    serde_json::from_str(include_str!("wonder_skills_off_baseline.json")).unwrap()
}

/// Cupola's preamble on a Codex turn that offers the thread tools.
fn preamble() -> Value {
    json!({"kind": "application", "value": crate::preamble::text(true)})
}

fn params_of<'a>(
    requests: &'a mut Value,
    method: &str,
) -> Vec<&'a mut serde_json::Map<String, Value>> {
    requests
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter(|request| request["method"] == method)
        .map(|request| request["params"].as_object_mut().unwrap())
        .collect()
}

// Contract: with the thread tools off, a new Codex thread starts without
// them and each Claude turn offers none (an empty list, which also removes
// tools an earlier turn offered); nothing else sent changes.
#[tokio::test]
async fn thread_tools_off_remove_only_the_tools() {
    let fx = fx().await;
    for family in [AgentFamily::Codex, AgentFamily::Claude] {
        fx.state
            .store
            .set_agent_thread_tools(family, false, "now")
            .await
            .unwrap();
    }
    converse(&fx).await;
    let mut actual = recorded(&fx);
    let mut codex = actual["codex"].take();
    let starts = params_of(&mut codex, "thread/start");
    assert_eq!(starts.len(), 1);
    for start in starts {
        assert!(start.get("dynamicTools").is_none());
    }
    for turn in params_of(&mut codex, "turn/start") {
        let preamble = &mut turn["additionalContext"][PREAMBLE_KEY]["value"];
        assert_eq!(*preamble, crate::preamble::text(false));
        *preamble = json!(crate::preamble::text(true));
    }
    let mut expected = baseline();
    for start in params_of(&mut expected["codex"], "thread/start") {
        assert!(start.remove("dynamicTools").is_some());
    }
    assert_eq!(codex, expected["codex"]);

    let mut claude = actual["claude"].take();
    let turns = params_of(&mut claude, "turn/start");
    assert_eq!(turns.len(), 2);
    for turn in turns {
        assert_eq!(turn["dynamicTools"], json!([]));
        turn.remove("dynamicTools");
        // Cupola's preamble no longer names the tools.
        assert_eq!(turn["wonderPreamble"], crate::preamble::text(false));
        turn.insert("wonderPreamble".into(), json!(crate::preamble::text(true)));
    }
    for turn in params_of(&mut expected["claude"], "turn/start") {
        assert!(turn.remove("dynamicTools").is_some());
    }
    assert_eq!(claude, expected["claude"]);
}

// Contract: an instruction that is on reaches every turn Cupola runs, from
// the phone or the Mac, through each harness's own field (Codex: an
// application entry in `additionalContext`; Claude: `wonderInstruction`,
// which the bridge appends to the system prompt), and nothing else changes.
// Saved but off, it sends nothing.
#[tokio::test]
async fn an_instruction_reaches_each_turn_only_while_on() {
    let text = "Answer in British English.";
    let fx = fx().await;
    for family in [AgentFamily::Codex, AgentFamily::Claude] {
        fx.state
            .store
            .set_agent_instruction(family, true, text, "now")
            .await
            .unwrap();
    }
    converse(&fx).await;
    let mut actual = recorded(&fx);
    let expected = baseline();

    let mut codex = actual["codex"].take();
    let turns = params_of(&mut codex, "turn/start");
    assert_eq!(turns.len(), 2);
    for turn in turns {
        assert_eq!(
            turn["additionalContext"],
            json!({INSTRUCTION_CONTEXT_KEY: {"kind": "application", "value": text},
                PREAMBLE_KEY: preamble()})
        );
        turn["additionalContext"]
            .as_object_mut()
            .unwrap()
            .remove(INSTRUCTION_CONTEXT_KEY);
    }
    assert_eq!(codex, expected["codex"]);

    let mut claude = actual["claude"].take();
    let turns = params_of(&mut claude, "turn/start");
    assert_eq!(turns.len(), 2);
    for turn in turns {
        assert_eq!(turn["wonderInstruction"], text);
        turn.remove("wonderInstruction");
    }
    assert_eq!(claude, expected["claude"]);

    // Off again: the text is kept and nothing is sent.
    let fx = crate::provider_switch_tests::fx().await;
    for family in [AgentFamily::Codex, AgentFamily::Claude] {
        fx.state
            .store
            .set_agent_instruction(family, false, text, "now")
            .await
            .unwrap();
    }
    converse(&fx).await;
    assert_eq!(recorded(&fx), baseline());
}

// Contract: with a Cupola skill on, a phone turn's Codex context carries the
// phone signal and the instruction side by side.
#[tokio::test]
async fn the_instruction_joins_the_phone_signal_on_codex() {
    use crate::wonder_skills::{PHONE_CONTEXT, SKILLS};
    let fx = fx().await;
    fx.state
        .store
        .set_wonder_skill(SKILLS[0].id, true, "now")
        .await
        .unwrap();
    fx.state
        .store
        .set_agent_instruction(AgentFamily::Codex, true, "Be brief.", "now")
        .await
        .unwrap();
    converse(&fx).await;
    let mut actual = recorded(&fx);
    let turns = params_of(&mut actual["codex"], "turn/start");
    assert_eq!(
        turns[0]["additionalContext"],
        json!({
            "wonder_phone": {"kind": "application", "value": PHONE_CONTEXT},
            INSTRUCTION_CONTEXT_KEY: {"kind": "application", "value": "Be brief."},
            PREAMBLE_KEY: preamble(),
        })
    );
    assert_eq!(
        turns[1]["additionalContext"],
        json!({INSTRUCTION_CONTEXT_KEY: {"kind": "application", "value": "Be brief."},
            PREAMBLE_KEY: preamble()})
    );
}

// Contract (owner, Oct 22): every turn Cupola runs carries Cupola's preamble,
// naming the thread tools while they are offered, through each harness's own
// channel: Codex an application entry in `additionalContext` (its own
// developer instructions stay the owner's), Claude `wonderPreamble`, which
// the bridge puts first in the system-prompt append.
#[tokio::test]
async fn cupolas_preamble_reaches_every_codex_and_claude_turn() {
    let fx = fx().await;
    converse(&fx).await;
    let mut actual = recorded(&fx);
    let codex = params_of(&mut actual["codex"], "turn/start");
    assert_eq!(codex.len(), 2);
    for turn in codex {
        assert_eq!(turn["additionalContext"][PREAMBLE_KEY], preamble());
        assert!(turn.get("developerInstructions").is_none());
    }
    for start in params_of(&mut actual["codex"], "thread/start") {
        assert!(start.get("developerInstructions").is_none());
    }
    let claude = params_of(&mut actual["claude"], "turn/start");
    assert_eq!(claude.len(), 2);
    for turn in claude {
        assert_eq!(turn["wonderPreamble"], crate::preamble::text(true));
    }
}
