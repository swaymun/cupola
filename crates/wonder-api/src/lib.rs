//! Versioned, client-facing Wonder contracts.

use serde::{Deserialize, Serialize};

pub mod pairing;
pub mod pairing_protocol;
pub mod rescue;

/// File-change presentation retained in history; version 2 preserves native moves.
pub const FILE_CHANGE_PROJECTION_VERSION: i64 = 2;

/// Keep event frames within Foundation's default WebSocket receive limit.
pub const EVENT_FRAME_MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    DraftOnDevice,
    Submitting,
    AcceptedByWonder,
    DispatchingToCodex,
    AcceptedByCodex,
    Streaming,
    Completed,
    Interrupted,
    Failed,
    Uncertain,
    SafeToRetry,
}

/// Which question "is a turn active?" is being asked. Each scope is one
/// answer; `DeliveryState::is_active` is the only place they are defined and
/// `turn_states_sql` renders the same lists for the store's queries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnScope {
    /// The provider has the turn and is running it.
    Running,
    /// Sent or running: at most one message per conversation is in flight.
    InFlight,
    /// Cupola owns an unfinished turn: queued, in flight or running.
    Admitted,
    /// A turn that may still be using the provider session: in flight, or
    /// delivery unconfirmed. Provider switch and rewind wait for it.
    Unsettled,
    /// Anything that keeps a thread from being archived or rewound:
    /// admitted work plus unconfirmed delivery.
    Held,
    /// States a turn-end event may still settle: running or unconfirmed.
    Settleable,
}

impl DeliveryState {
    pub const ALL: [DeliveryState; 11] = [
        DeliveryState::DraftOnDevice,
        DeliveryState::Submitting,
        DeliveryState::AcceptedByWonder,
        DeliveryState::DispatchingToCodex,
        DeliveryState::AcceptedByCodex,
        DeliveryState::Streaming,
        DeliveryState::Completed,
        DeliveryState::Interrupted,
        DeliveryState::Failed,
        DeliveryState::Uncertain,
        DeliveryState::SafeToRetry,
    ];

    /// The name stored in the database and sent on the wire.
    pub const fn as_str(&self) -> &'static str {
        match self {
            DeliveryState::DraftOnDevice => "draft_on_device",
            DeliveryState::Submitting => "submitting",
            DeliveryState::AcceptedByWonder => "accepted_by_wonder",
            DeliveryState::DispatchingToCodex => "dispatching_to_codex",
            DeliveryState::AcceptedByCodex => "accepted_by_codex",
            DeliveryState::Streaming => "streaming",
            DeliveryState::Completed => "completed",
            DeliveryState::Interrupted => "interrupted",
            DeliveryState::Failed => "failed",
            DeliveryState::Uncertain => "uncertain",
            DeliveryState::SafeToRetry => "safe_to_retry",
        }
    }

    pub fn from_name(name: &str) -> Option<DeliveryState> {
        DeliveryState::ALL
            .into_iter()
            .find(|state| state.as_str() == name)
    }

    pub const fn is_active(&self, scope: TurnScope) -> bool {
        use DeliveryState::*;
        match scope {
            TurnScope::Running => matches!(self, AcceptedByCodex | Streaming),
            TurnScope::InFlight => matches!(self, DispatchingToCodex | AcceptedByCodex | Streaming),
            TurnScope::Admitted => matches!(
                self,
                AcceptedByWonder | DispatchingToCodex | AcceptedByCodex | Streaming
            ),
            TurnScope::Unsettled => matches!(
                self,
                DispatchingToCodex | AcceptedByCodex | Streaming | Uncertain
            ),
            TurnScope::Held => matches!(
                self,
                AcceptedByWonder | DispatchingToCodex | AcceptedByCodex | Streaming | Uncertain
            ),
            TurnScope::Settleable => matches!(self, AcceptedByCodex | Streaming | Uncertain),
        }
    }

    /// Whether a message in this state may move to `next`. Completed and
    /// interrupted are final; a failed or never-sent message can only be
    /// queued again; an unconfirmed one can be found running, settled, or
    /// taken back as a Guide. `next == self` is always allowed (idempotent).
    pub const fn may_become(&self, next: DeliveryState) -> bool {
        use DeliveryState::*;
        if *self as u8 == next as u8 {
            return true;
        }
        match self {
            DraftOnDevice => matches!(next, Submitting | AcceptedByWonder | Failed),
            Submitting => matches!(
                next,
                AcceptedByWonder | Failed | SafeToRetry | Uncertain | Interrupted
            ),
            AcceptedByWonder => matches!(
                next,
                DispatchingToCodex
                    | AcceptedByCodex
                    | Streaming
                    | Completed
                    | Interrupted
                    | Failed
                    | SafeToRetry
                    | Uncertain
            ),
            DispatchingToCodex => matches!(
                next,
                AcceptedByWonder
                    | AcceptedByCodex
                    | Streaming
                    | Completed
                    | Interrupted
                    | Failed
                    | SafeToRetry
                    | Uncertain
            ),
            AcceptedByCodex | Streaming => matches!(
                next,
                AcceptedByCodex | Streaming | Completed | Interrupted | Failed | Uncertain
            ),
            Uncertain => matches!(
                next,
                AcceptedByWonder | AcceptedByCodex | Streaming | Completed | Interrupted | Failed
            ),
            SafeToRetry | Failed => matches!(next, AcceptedByWonder | Interrupted),
            Completed | Interrupted => false,
        }
    }
}

/// `'a','b'` for the states in `scope`, for `state IN (...)` in a query.
pub fn turn_states_sql(scope: TurnScope) -> &'static str {
    static LISTS: std::sync::OnceLock<[String; 6]> = std::sync::OnceLock::new();
    let lists = LISTS.get_or_init(|| {
        [
            TurnScope::Running,
            TurnScope::InFlight,
            TurnScope::Admitted,
            TurnScope::Unsettled,
            TurnScope::Held,
            TurnScope::Settleable,
        ]
        .map(|scope| {
            DeliveryState::ALL
                .iter()
                .filter(|state| state.is_active(scope))
                .map(|state| format!("'{}'", state.as_str()))
                .collect::<Vec<_>>()
                .join(",")
        })
    });
    &lists[match scope {
        TurnScope::Running => 0,
        TurnScope::InFlight => 1,
        TurnScope::Admitted => 2,
        TurnScope::Unsettled => 3,
        TurnScope::Held => 4,
        TurnScope::Settleable => 5,
    }]
}

/// `'a','b'` for the states a message may leave to become `next`.
pub fn states_before_sql(next: DeliveryState) -> String {
    DeliveryState::ALL
        .iter()
        .filter(|state| state.may_become(next))
        .map(|state| format!("'{}'", state.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientMessageReceipt {
    pub client_message_id: String,
    pub wonder_message_id: String,
    pub body_sha256: String,
    pub conversation_id: String,
    pub delivery_state: DeliveryState,
    pub codex_thread_id: Option<String>,
    pub codex_turn_id: Option<String>,
    /// Whether the message waits in its conversation's queue (behind a
    /// running turn or earlier messages, or while the Mac holds the chat)
    /// rather than being sent now. Absent where it does not apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventCursor {
    pub host_epoch: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostEventEnvelope {
    #[serde(alias = "event_id")]
    pub event_id: String,
    #[serde(alias = "host_epoch")]
    pub host_epoch: String,
    pub sequence: u64,
    #[serde(alias = "occurred_at")]
    pub occurred_at: String,
    #[serde(alias = "request_id")]
    pub request_id: Option<String>,
    #[serde(alias = "device_id")]
    pub device_id: Option<String>,
    #[serde(alias = "conversation_id")]
    pub conversation_id: Option<String>,
    #[serde(alias = "message_id")]
    pub message_id: Option<String>,
    #[serde(alias = "thread_id")]
    pub thread_id: Option<String>,
    #[serde(alias = "turn_id")]
    pub turn_id: Option<String>,
    #[serde(alias = "item_id")]
    pub item_id: Option<String>,
    #[serde(alias = "approval_id")]
    pub approval_id: Option<String>,
    pub event: WonderEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WonderEvent {
    HostStatus {
        state: String,
    },
    MessageState {
        state: DeliveryState,
    },
    AssistantDelta {
        text: String,
    },
    AssistantCompleted {
        text: String,
    },
    ComputerUseScreenshot {
        image_url: String,
    },
    ComputerSessionChanged {
        session_id: String,
        generation: u64,
        state: String,
    },
    Activity {
        category: String,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    TerminalError {
        message: String,
    },
    ApprovalOpened {
        request_id: String,
    },
    ApprovalResolved {
        request_id: String,
        decision: String,
    },
    ResyncRequired {
        reason: ResyncReason,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    EpochChanged,
    ReplayWindowExpired,
}

#[cfg(test)]
mod tests {
    use super::{states_before_sql, turn_states_sql, DeliveryState, TurnScope};

    #[test]
    fn delivery_state_names_match_the_wire_and_scopes_nest() {
        for state in DeliveryState::ALL {
            let wire = serde_json::to_string(&state).unwrap();
            assert_eq!(wire, format!("\"{}\"", state.as_str()));
            assert_eq!(DeliveryState::from_name(state.as_str()), Some(state));
        }
        assert_eq!(DeliveryState::from_name("nonsense"), None);
        assert_eq!(
            turn_states_sql(TurnScope::Running),
            "'accepted_by_codex','streaming'"
        );
        assert_eq!(
            turn_states_sql(TurnScope::Held),
            "'accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming','uncertain'"
        );
        for state in DeliveryState::ALL {
            // Narrower scopes never admit a state a wider one refuses.
            if state.is_active(TurnScope::Running) {
                assert!(state.is_active(TurnScope::InFlight));
            }
            if state.is_active(TurnScope::InFlight) {
                assert!(state.is_active(TurnScope::Admitted));
                assert!(state.is_active(TurnScope::Unsettled));
            }
            if state.is_active(TurnScope::Admitted) || state.is_active(TurnScope::Unsettled) {
                assert!(state.is_active(TurnScope::Held));
            }
        }
    }

    #[test]
    fn final_messages_stay_final_and_active_ones_can_settle() {
        use DeliveryState::*;
        for from in [Completed, Interrupted] {
            for to in DeliveryState::ALL {
                assert_eq!(from.may_become(to), from == to, "{from:?} -> {to:?}");
            }
        }
        for from in DeliveryState::ALL {
            if from.is_active(TurnScope::Held) {
                for settled in [Completed, Interrupted] {
                    assert!(from.may_become(settled), "{from:?} -> {settled:?}");
                }
            }
        }
        assert!(!Uncertain.may_become(SafeToRetry));
        assert!(!states_before_sql(Completed).contains("'interrupted'"));
        assert!(states_before_sql(AcceptedByWonder).contains("'failed'"));
    }

    use super::*;

    #[test]
    fn unknown_state_events_fail_while_known_events_tolerate_extra_fields() {
        assert!(serde_json::from_value::<WonderEvent>(serde_json::json!({
            "type": "future_approval", "data": {"decision": "approved"}
        }))
        .is_err());
        let event = serde_json::from_value::<WonderEvent>(serde_json::json!({
            "type": "host_status", "data": {"state": "ready", "futureField": true}
        }))
        .unwrap();
        assert_eq!(
            event,
            WonderEvent::HostStatus {
                state: "ready".into()
            }
        );
    }

    #[test]
    fn host_event_envelope_uses_the_browser_contract_casing() {
        let envelope = HostEventEnvelope {
            event_id: "event-1".into(),
            host_epoch: "epoch-1".into(),
            sequence: 1,
            occurred_at: "2026-09-02T00:00:00Z".into(),
            request_id: None,
            device_id: None,
            conversation_id: Some("project-thread-1".into()),
            message_id: None,
            thread_id: None,
            turn_id: None,
            item_id: Some("item-1".into()),
            approval_id: None,
            event: WonderEvent::AssistantCompleted {
                text: "done".into(),
            },
        };
        let value = serde_json::to_value(envelope).expect("event should serialize");
        assert_eq!(value["eventId"], "event-1");
        assert_eq!(value["occurredAt"], "2026-09-02T00:00:00Z");
        assert_eq!(value["conversationId"], "project-thread-1");
        assert!(value.get("event_id").is_none());
    }

    #[test]
    fn host_event_envelope_reads_legacy_snake_case_rows() {
        let value = serde_json::json!({
            "event_id": "event-legacy",
            "host_epoch": "epoch-legacy",
            "sequence": 2,
            "occurred_at": "2026-09-02T00:00:00Z",
            "request_id": null,
            "device_id": null,
            "conversation_id": "project-thread-1",
            "message_id": null,
            "thread_id": null,
            "turn_id": null,
            "item_id": null,
            "approval_id": null,
            "event": {"type": "assistant_completed", "data": {"text": "done"}}
        });
        let envelope: HostEventEnvelope = serde_json::from_value(value).expect("legacy event");
        assert_eq!(envelope.event_id, "event-legacy");
        assert_eq!(
            envelope.conversation_id.as_deref(),
            Some("project-thread-1")
        );
    }
}
