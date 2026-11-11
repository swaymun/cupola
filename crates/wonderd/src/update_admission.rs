//! Fence new work while the host persists and pauses update continuations.
//! Cancellation, expiry or a replacement daemon recovers the durable handoff.
use crate::{AppState, LocalOwnerAuthority};
use axum::{
    body::Body,
    extract::{Extension, State},
    http::{Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

const LEASE_DURATION: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct UpdateAdmission {
    lease: Mutex<Option<Lease>>,
    paused: AtomicBool,
}

pub(crate) struct Lease {
    request_id: String,
    expires: Instant,
}

impl UpdateAdmission {
    fn current(lease: &mut Option<Lease>) -> bool {
        if lease
            .as_ref()
            .is_some_and(|value| value.expires <= Instant::now())
        {
            *lease = None;
        }
        lease.is_some()
    }

    /// Scheduler claims use the same barrier as HTTP admissions.
    pub(crate) async fn claim_guard(&self) -> Option<tokio::sync::MutexGuard<'_, Option<Lease>>> {
        let mut lease = self.lease.lock().await;
        if Self::current(&mut lease) {
            None
        } else {
            self.paused.store(false, Ordering::Release);
            Some(lease)
        }
    }

    pub(crate) fn work_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Background automation tasks can reach dispatch after the HTTP
    /// admission has finished. Fence their native submits at the shared lock.
    pub(crate) async fn dispatch_guard<'a>(
        &self,
        lock: &'a Mutex<()>,
    ) -> tokio::sync::MutexGuard<'a, ()> {
        loop {
            while self.work_paused() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let guard = lock.lock().await;
            if !self.work_paused() {
                return guard;
            }
        }
    }

    /// Pauses whatever runs (tests; the HTTP route passes the consent).
    #[cfg(test)]
    pub(crate) async fn prepare_update(
        &self,
        request_id: &str,
        state: &AppState,
    ) -> Result<(), (StatusCode, String)> {
        self.prepare_consented_update(request_id, None, state).await
    }

    /// `consented_running` is how many running replies the owner agreed to
    /// interrupt; more than that (work that started after they were asked)
    /// refuses before anything is paused. `None` pauses whatever runs.
    pub(crate) async fn prepare_consented_update(
        &self,
        request_id: &str,
        consented_running: Option<usize>,
        state: &AppState,
    ) -> Result<(), (StatusCode, String)> {
        let mut lease = self.lease.lock().await;
        if Self::current(&mut lease) && lease.as_ref().is_some_and(|l| l.request_id != request_id) {
            return Err((
                StatusCode::CONFLICT,
                "Another update is being prepared.".into(),
            ));
        }
        // Answer before the caller's 30 s HTTP timeout when a chat update is
        // still being saved; the caller retries.
        let Ok(_dispatch) =
            tokio::time::timeout(Duration::from_secs(20), state.dispatch_lock.lock()).await
        else {
            return Err((
                StatusCode::CONFLICT,
                "Cupola is still saving a chat update. The update will retry.".into(),
            ));
        };
        // New work is admitted only through `claim_guard` and dispatch, and
        // both locks are held here, so this count cannot grow before the pause.
        if let Some(consented) = consented_running {
            let running = running_replies(state).await.map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Cupola could not check what is running.".to_owned(),
                )
            })?;
            if running > consented {
                return Err((
                    StatusCode::CONFLICT,
                    format!("{WORK_STARTED} ({running} running)."),
                ));
            }
        }
        self.paused.store(true, Ordering::Release);
        *lease = Some(Lease {
            request_id: request_id.into(),
            expires: Instant::now() + LEASE_DURATION,
        });
        let paused = tokio::time::timeout(
            Duration::from_secs(25),
            crate::update_handoff::pause(state, request_id),
        )
        .await;
        match paused {
            Ok(Ok(())) => {
                lease.as_mut().unwrap().expires = Instant::now() + LEASE_DURATION;
                Ok(())
            }
            result => {
                *lease = None;
                self.paused.store(false, Ordering::Release);
                let message = match result {
                    Ok(Err(message)) => message,
                    _ => "Work could not be safely paused. The update will retry.".into(),
                };
                Err((StatusCode::CONFLICT, message))
            }
        }
    }

    pub(crate) async fn cancel_lease(&self, request_id: &str) -> bool {
        let mut lease = self.lease.lock().await;
        if Self::current(&mut lease) {
            if lease
                .as_ref()
                .is_some_and(|value| value.request_id != request_id)
            {
                return false;
            }
            *lease = None;
        }
        self.paused.store(false, Ordering::Release);
        true
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRequest {
    request_id: String,
    /// Running replies the owner agreed to pause (`wonder update`).
    #[serde(default)]
    consented_running: Option<usize>,
}

/// Starts the refusal `wonder update` recognizes as "ask again".
pub(crate) const WORK_STARTED: &str = "Agents started working after you were asked";

/// Replies an update would pause; "uncertain" ones are not running.
async fn running_replies(state: &AppState) -> Result<usize, ()> {
    let messages = state
        .store
        .active_runtime_messages()
        .await
        .map_err(|_| ())?;
    Ok(messages.iter().filter(|m| m.state != "uncertain").count())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Prepared {
    ready: bool,
    request_id: String,
    expires_at: String,
}

pub(crate) async fn prepare(
    State(state): State<AppState>,
    local: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<UpdateRequest>,
) -> Response {
    if local.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if uuid::Uuid::parse_str(&request.request_id).is_err() {
        return (StatusCode::BAD_REQUEST, "requestId must be a UUID").into_response();
    }
    match state
        .update_admission
        .prepare_consented_update(&request.request_id, request.consented_running, &state)
        .await
    {
        Ok(()) => {}
        Err(error) => return error.into_response(),
    }
    let expires_at =
        (Utc::now() + ChronoDuration::seconds(60)).to_rfc3339_opts(SecondsFormat::Millis, true);
    Json(Prepared {
        ready: true,
        request_id: request.request_id,
        expires_at,
    })
    .into_response()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Work {
    /// Replies running now; an update pauses them and continues them after.
    running: usize,
    /// Work an update cannot pause (computer sharing, an unaccepted send).
    blocks_update: bool,
}

/// `GET /api/v1/host/update/work`: what a restart would interrupt, without
/// pausing anything, so `wonder update` can ask first.
pub(crate) async fn work(
    State(state): State<AppState>,
    local: Option<Extension<LocalOwnerAuthority>>,
) -> Response {
    if local.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    let running = running_replies(&state).await;
    let blocked = state
        .store
        .has_unsafe_update_work(&state.host_installation_id)
        .await;
    match (running, blocked) {
        (Ok(running), Ok(blocks_update)) => Json(Work {
            running,
            blocks_update,
        })
        .into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn cancel(
    State(state): State<AppState>,
    local: Option<Extension<LocalOwnerAuthority>>,
    Json(request): Json<UpdateRequest>,
) -> Response {
    if local.is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if uuid::Uuid::parse_str(&request.request_id).is_err() {
        return (StatusCode::BAD_REQUEST, "requestId must be a UUID").into_response();
    }
    if !state
        .update_admission
        .cancel_lease(&request.request_id)
        .await
    {
        return StatusCode::CONFLICT.into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

fn admits_work(method: &Method, path: &str) -> bool {
    if method == Method::PUT
        && path.starts_with("/api/v1/conversations/")
        && path.ends_with("/goal")
    {
        return true;
    }
    if method != Method::POST {
        return false;
    }
    // These routes persist a new user turn, automation, or computer
    // session/control lease. Project creation
    // checks this lease inside create_thread after exact receipt replay.
    (path.starts_with("/api/v1/conversations/")
        && (path.ends_with("/messages") || path.ends_with("/steer")))
        || (path.starts_with("/api/v1/messages/") && path.ends_with("/retry"))
        || (path.starts_with("/api/v1/automations/") && path.ends_with("/run"))
        || path == "/api/v1/computer/sessions"
        || (path.starts_with("/api/v1/computer/sessions/")
            && (path.ends_with("/admission")
                || path.ends_with("/control/acquire")
                || path.ends_with("/control/resume")))
}

pub(crate) async fn guard_new_work(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if !admits_work(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let Some(_guard) = state.update_admission.claim_guard().await else {
        return (
            StatusCode::CONFLICT,
            "Cupola is preparing to update. Try again shortly.",
        )
            .into_response();
    };
    next.run(request).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[test]
    fn gate_covers_all_work_admissions() {
        for path in [
            "/api/v1/conversations/id/messages",
            "/api/v1/messages/id/retry",
            "/api/v1/automations/id/run",
            "/api/v1/computer/sessions",
            "/api/v1/computer/sessions/id/admission",
            "/api/v1/computer/sessions/id/control/acquire",
        ] {
            assert!(admits_work(&Method::POST, path), "{path}");
        }
        assert!(!admits_work(&Method::POST, "/api/v1/projects/id/threads"));
        assert!(admits_work(&Method::PUT, "/api/v1/conversations/id/goal"));
        assert!(!admits_work(
            &Method::DELETE,
            "/api/v1/conversations/id/goal"
        ));
        assert!(!admits_work(&Method::POST, "/api/v1/host/update/cancel"));
        assert!(!admits_work(
            &Method::GET,
            "/api/v1/conversations/id/messages"
        ));
    }

    #[tokio::test]
    async fn preparation_waits_for_admission_and_preserves_queued_work() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let store = &state.store;
        let admission = &state.update_admission;
        let in_flight = admission.claim_guard().await.unwrap();
        let preparing = admission.prepare_update("request", &state);
        tokio::pin!(preparing);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut preparing)
                .await
                .is_err()
        );
        store
            .insert_dispatch_message(
                "owner",
                "client",
                "body",
                "hash",
                "d19b0f7e-97bb-45d7-8206-324a62386dd9",
                &[],
                "now",
                true,
            )
            .await
            .unwrap();
        drop(in_flight);
        assert_eq!(preparing.await, Ok(()));
        assert_eq!(store.pending_dispatch_messages().await.unwrap().len(), 1);
        assert!(admission.claim_guard().await.is_none());
    }

    #[tokio::test]
    async fn lease_blocks_admission_until_cancel_or_expiry() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let admission = &state.update_admission;
        admission.prepare_update("first", &state).await.unwrap();
        assert!(admission.claim_guard().await.is_none());
        assert_eq!(
            admission.prepare_update("second", &state).await,
            Err((
                StatusCode::CONFLICT,
                "Another update is being prepared.".into()
            ))
        );
        assert!(!admission.cancel_lease("second").await);
        assert!(admission.claim_guard().await.is_none());
        admission.prepare_update("first", &state).await.unwrap();
        assert!(admission.cancel_lease("first").await);
        assert!(admission.claim_guard().await.is_some());
        admission.prepare_update("first", &state).await.unwrap();
        {
            let mut lease = admission.lease.lock().await;
            lease.as_mut().unwrap().expires = Instant::now() - Duration::from_secs(1);
        }
        assert!(admission.claim_guard().await.is_some());
    }

    #[tokio::test]
    async fn prepared_update_rejects_goal_mutation_but_allows_goal_clear() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        {
            let mut lease = state.update_admission.lease.lock().await;
            *lease = Some(Lease {
                request_id: "update".into(),
                expires: Instant::now() + Duration::from_secs(60),
            });
            state.update_admission.paused.store(true, Ordering::Release);
        }
        let put = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/api/v1/conversations/missing/goal")
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"objective":"Start work"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::CONFLICT);
        let clear = crate::router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/v1/conversations/missing/goal")
                    .header("x-wonder-loopback-capability", &state.loopback_capability)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(clear.status(), StatusCode::NOT_FOUND);
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn work_report_is_local_owner_only_and_pauses_nothing() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let get = |capability: Option<String>| {
            let mut request = Request::builder().uri("/api/v1/host/update/work");
            if let Some(capability) = capability {
                request = request.header("x-wonder-loopback-capability", capability);
            }
            crate::router(state.clone()).oneshot(request.body(Body::empty()).unwrap())
        };
        let remote = get(None).await.unwrap();
        assert!(remote.status().is_client_error(), "{}", remote.status());
        let local = get(Some(state.loopback_capability.clone())).await.unwrap();
        assert_eq!(local.status(), StatusCode::OK);
        let body = axum::body::to_bytes(local.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"running": 0, "blocksUpdate": false})
        );
        assert!(!state.update_admission.work_paused());
        assert!(state.update_admission.claim_guard().await.is_some());
        state.projects.codex.lock().await.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn background_dispatch_waits_for_cancel_and_remote_clients_have_no_update_authority() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        let request = uuid::Uuid::new_v4().to_string();
        let response = prepare(
            State(state.clone()),
            None,
            Json(UpdateRequest {
                request_id: request.clone(),
                consented_running: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        state
            .update_admission
            .prepare_update(&request, &state)
            .await
            .unwrap();
        let dispatch = state.update_admission.dispatch_guard(&state.dispatch_lock);
        tokio::pin!(dispatch);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut dispatch)
                .await
                .is_err()
        );
        assert!(state.update_admission.cancel_lease(&request).await);
        assert!(tokio::time::timeout(Duration::from_secs(60), &mut dispatch)
            .await
            .is_ok());
    }
}
