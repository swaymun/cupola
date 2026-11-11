//! A T3 thread's reply on the open chat, read through T3 Code's MCP tools
//! instead of watching the transcript file T3 Code's agent writes. While a
//! run is active (one stream per T3 thread, started by a send into it and by
//! the phone reading a thread T3 Code runs), the stream pages
//! `t3_thread_read` forward from where it last stopped (`nextPosition`, else
//! the newest item's position) and waits for more with `t3_thread_wait`. New
//! items wake the conversation's `native_history`, so the open chat reads them;
//! nothing here carries message content. The stream starts at the timeline's
//! end, so history that was already there wakes nothing. When T3 Code reports
//! no active run, one more read after `SETTLE` collects what landed last, then
//! the stream stops. A read that fails changes nothing; `GIVE_UP` in a row
//! stop it (every exit does the final refresh), and the next send or phone
//! read starts it again.
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use super::*;

/// Longest a `t3_thread_wait` blocks, so a reply streams in steps.
const WAIT_STEP: Duration = Duration::from_millis(1500);
const SETTLE: Duration = Duration::from_millis(1500);
const GIVE_UP: u32 = 12;
/// Pages one drain reads before it waits, so a long history cannot hold the loop.
const MAX_PAGES: u32 = 50;
/// Longest a stream outlives its start, whatever T3 Code says of the run.
const STREAM_LIMIT: Duration = Duration::from_secs(12 * 60 * 60);

/// (conversation, T3 thread) pairs with a stream.
static STREAMING: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Default::default);

struct Streaming(String, String);

impl Drop for Streaming {
    fn drop(&mut self) {
        STREAMING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(self.0.clone(), self.1.clone()));
    }
}

/// Streams `thread`'s timeline into `conversation` until its run ends.
pub(crate) fn watch(state: &AppState, conversation: &str, thread: &str) {
    if cfg!(test) {
        return;
    }
    if !STREAMING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((conversation.to_owned(), thread.to_owned()))
    {
        return;
    }
    let guard = Streaming(conversation.to_owned(), thread.to_owned());
    let (state, conversation, thread) = (state.clone(), conversation.to_owned(), thread.to_owned());
    tokio::spawn(async move {
        let _guard = guard;
        run(&state, &conversation, &thread).await;
    });
}

/// The stream's reads, then the final refresh on every way out.
async fn run(state: &AppState, conversation: &str, thread: &str) {
    let mut cursor: Option<i64> = None;
    let mut caught_up = false;
    let mut failures = 0;
    let mut settled = false;
    let mut pages = 0;
    let mut ran = false;
    let deadline = tokio::time::Instant::now() + STREAM_LIMIT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let read = match state.t3_code.read_page(thread, cursor, None).await {
            Ok(read) => read,
            Err(CallError::NotConnected) => break,
            Err(_) => {
                failures += 1;
                if failures >= GIVE_UP {
                    break;
                }
                tokio::time::sleep(SETTLE).await;
                continue;
            }
        };
        failures = 0;
        let items = read.get("items").and_then(Value::as_array);
        let fresh = items.is_some_and(|items| !items.is_empty());
        let newest = read
            .get("nextPosition")
            .and_then(Value::as_i64)
            .or_else(|| {
                items?
                    .iter()
                    .filter_map(|item| item.get("position").and_then(Value::as_i64))
                    .max()
            });
        cursor = cursor.max(newest);
        pages += 1;
        let more = read.get("hasMore").and_then(Value::as_bool) == Some(true) && fresh;
        if more && pages < MAX_PAGES {
            continue;
        }
        pages = 0;
        // The first drain only finds the end; later ones are news.
        if fresh && caught_up {
            wake(state, conversation).await;
        }
        caught_up = true;
        let run = read
            .pointer("/thread/activeRunId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match run {
            Some(run) => {
                settled = false;
                ran = true;
                // Returns early when the run ends; the next read drains it.
                // T3 Code may answer at once, or fail: wait a moment then.
                let started = Instant::now();
                let ended = state.t3_code.run_ended(thread, &run, WAIT_STEP).await;
                if !matches!(ended, Ok(true)) && started.elapsed() < WAIT_STEP / 3 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            None if !settled => {
                settled = true;
                tokio::time::sleep(SETTLE).await;
            }
            None => break,
        }
    }
    // The transcript can land just after T3 Code's last item: read once more.
    wake(state, conversation).await;
    #[cfg(test)]
    FINISHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(conversation.to_owned());
    // The sidebar's status follows the run's end now, not at the next
    // `external_push` poll (4 s); a stream that saw no run says nothing.
    if ran {
        crate::external_push::publish_status(state, conversation).await;
    }
}

/// Conversations whose stream reached its final refresh (tests).
#[cfg(test)]
static FINISHED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// New items for the open chat: an OpenCode or Cursor chat has no transcript,
/// so its timeline is read again; any other wakes its `native_history`.
async fn wake(state: &AppState, conversation: &str) {
    if matches!(
        state.store.t3_native_thread(conversation).await,
        Ok(Some(_))
    ) {
        let _ = crate::refresh_history(
            axum::extract::State(state.clone()),
            axum::extract::Path(conversation.to_owned()),
        )
        .await;
    } else {
        crate::transcript_watch::wake(state, conversation).await;
    }
}

/// Whether a stream reads this conversation's T3 thread now. The transcript
/// watcher stays silent for it: T3 Code's own word on new items is enough.
pub(crate) fn streams(conversation: &str) -> bool {
    STREAMING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(streaming, _)| streaming == conversation)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Contract: a stream that ends because T3 Code is not connected still
    // does the final refresh (it returned before it once).
    #[tokio::test]
    async fn a_stream_that_cannot_read_still_does_its_final_refresh() {
        let (_dir, state) = crate::ingestion::tests::fixture().await;
        assert!(!state.t3_code.connected());
        run(&state, "stream-exit-conversation", "t3-thread").await;
        assert!(FINISHED
            .lock()
            .unwrap()
            .iter()
            .any(|id| id == "stream-exit-conversation"));
    }
}
