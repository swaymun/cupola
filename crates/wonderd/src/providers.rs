//! Per-provider adapters that need host state. The provider facts themselves
//! live in `wonder_store::ProviderDescriptor`; these functions are the only
//! places that map a family to a store, client or session identity, so a new
//! provider needs one arm in each.

use crate::AppState;
use wonder_app_server::RpcClient;
use wonder_store::{AgentFamily, NativeSessionKey, RuntimeBinding};

/// The provider history a Project thread of `family` lives in.
pub(crate) fn provider_store(state: &AppState, family: AgentFamily) -> &str {
    match family {
        AgentFamily::Codex => &state.projects.codex_store,
        AgentFamily::Claude => &state.projects.claude_store,
        // OpenCode keeps its sessions in its own database; Wonder's bridge
        // journal is the only store Wonder writes.
        AgentFamily::OpenCode => "opencode",
        AgentFamily::Cursor => "cursor",
    }
}

/// Whether `binding` carries the provider's own `native_session`. Codex binds
/// the provider thread itself. Claude binds Wonder's bridge thread, whose
/// session is the native Claude Code session.
pub(crate) fn binds_native_session(
    family: AgentFamily,
    native_session: &str,
    binding: &RuntimeBinding,
) -> bool {
    match family.provider().native_session {
        NativeSessionKey::ThreadId => binding.thread_id == native_session,
        NativeSessionKey::BridgeSessionId => binding.session_id.as_deref() == Some(native_session),
    }
}

/// History from a different provider home is retained, but must never be
/// resumed with this computer's sign-in or silently moved to another home.
pub(crate) fn other_history(family: AgentFamily) -> String {
    format!("This thread used a separate {} sign-in. Added accounts are no longer supported. Its saved history is read-only; its files remain on your computer.", family.provider().display_name)
}

pub(crate) fn is_default_store(state: &AppState, family: AgentFamily, store: &str) -> bool {
    store == provider_store(state, family)
}

/// Resolve only the computer's own provider runtime; legacy stores fail closed.
pub(crate) async fn rpc_for_store(
    state: &AppState,
    family: AgentFamily,
    store: &str,
) -> Result<RpcClient, String> {
    if !is_default_store(state, family, store) {
        return Err(other_history(family));
    }
    crate::projects::rpc_for(state, family).await
}

pub(crate) async fn live_rpc_for_store(
    state: &AppState,
    family: AgentFamily,
    store: &str,
) -> Option<RpcClient> {
    if !is_default_store(state, family, store) {
        return None;
    }
    crate::projects::live_rpc_for(state, family).await
}

/// Desktop Codex reads are allowed only for the computer's own history.
pub(crate) async fn home_for_store(
    state: &AppState,
    family: AgentFamily,
    store: &str,
) -> Option<std::path::PathBuf> {
    is_default_store(state, family, store).then(|| state.codex_home.clone())
}
