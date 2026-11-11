//! The computer's name in Wonder: `GET /api/v1/host/name` and
//! `PUT /api/v1/host/name` `{"name": string | null}`.
//!
//! wonderd owns the one name the Mac app and the phone show and change. A name
//! the owner chose wins; otherwise it is the computer's own name, read when
//! wonderd starts. `/api/v1/host/status` reports the same `hostName`.
use crate::{projects, AppState, OwnerAuthority};
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Deserialize;
use serde_json::json;
use wonder_store::HOST_NAME_MAX_CHARS;

/// The name to show: the owner's choice, else the computer's own.
pub(crate) async fn current(state: &AppState) -> String {
    match state.store.host_name().await {
        Ok(Some(name)) => name,
        _ => crate::system_host_name().to_owned(),
    }
}

async fn reply(state: &AppState) -> Response {
    let Ok(chosen) = state.store.host_name().await else {
        return projects::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cupola couldn’t read this computer’s name. Try again.",
        );
    };
    let system = crate::system_host_name();
    Json(json!({
        "name": chosen.as_deref().unwrap_or(system),
        "systemName": system,
        "custom": chosen.is_some(),
        "maxLength": HOST_NAME_MAX_CHARS,
    }))
    .into_response()
}

pub(crate) async fn get_name(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
) -> Response {
    reply(&state).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetHostName {
    name: Option<String>,
}

/// Saves the owner's name; an empty or missing name goes back to the
/// computer's own.
pub(crate) async fn set_name(
    State(state): State<AppState>,
    Extension(_authority): Extension<OwnerAuthority>,
    Json(request): Json<SetHostName>,
) -> Response {
    let name = request
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(name) = name {
        if name.chars().count() > HOST_NAME_MAX_CHARS || name.chars().any(char::is_control) {
            return projects::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Keep the name to {HOST_NAME_MAX_CHARS} characters on one line."),
            );
        }
    }
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if state.store.set_host_name(name, &now).await.is_err() {
        return projects::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The name could not be saved. Try again.",
        );
    }
    reply(&state).await
}

#[cfg(all(test, unix))]
mod tests {
    use crate::test_support::{call, fixture};
    use axum::http::StatusCode;
    use serde_json::{json, Value};

    // Contract: the name starts as the computer's own; a chosen name is what
    // both the name route and host status report; clearing it goes back; a
    // name too long or over several lines is refused.
    #[tokio::test]
    async fn chosen_name_is_the_one_host_status_reports() {
        let (_dir, state) = fixture().await;
        let system = crate::system_host_name();
        let read = call(&state, "GET", "/api/v1/host/name", Value::Null).await;
        assert_eq!(read.0, StatusCode::OK, "{}", read.1);
        crate::tests::validate_http_contract("hostNameSettings", &read.1);
        assert_eq!(read.1["name"], system);
        assert_eq!(read.1["custom"], false);

        let set = call(
            &state,
            "PUT",
            "/api/v1/host/name",
            json!({"name": "  Studio Mac\n"}),
        )
        .await;
        assert_eq!(set.0, StatusCode::OK, "{}", set.1);
        crate::tests::validate_http_contract("hostNameSettings", &set.1);
        assert_eq!(
            (set.1["name"].as_str(), set.1["custom"].as_bool()),
            (Some("Studio Mac"), Some(true))
        );
        let status = call(&state, "GET", "/api/v1/host/status", Value::Null).await;
        assert_eq!(status.1["hostName"], "Studio Mac");

        for refused in [
            json!({"name": "x".repeat(61)}),
            json!({"name": "Two\nlines"}),
        ] {
            let reply = call(&state, "PUT", "/api/v1/host/name", refused).await;
            assert_eq!(reply.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", reply.1);
        }
        let cleared = call(&state, "PUT", "/api/v1/host/name", json!({"name": null})).await;
        assert_eq!(
            (cleared.1["name"].as_str(), cleared.1["custom"].as_bool()),
            (Some(system), Some(false))
        );
        let status = call(&state, "GET", "/api/v1/host/status", Value::Null).await;
        assert_eq!(status.1["hostName"], system);
    }
}
