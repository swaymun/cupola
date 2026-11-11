//! `wonder_code_thread`: an agent searches T3 Code's threads and organizes,
//! renames, forks or reads one, by calling `t3_code::threads` (T3 Code's MCP
//! tools) in-process. It does not depend on which side runs the agent's own
//! turn: a T3-run turn has no Cupola tools at all, and a Cupola-run turn is
//! not a session T3 Code holds. So the thread is named by `threadId` (from
//! `search`); only when it is left out does the tool fall back to the T3
//! thread that continues this chat. T3 Code has no delete tool here.
//!
//! Scope: like Cupola's other thread tools, it reaches only this Project's
//! own work: T3 projects whose folder is one of this Project's folders (and
//! their worktrees). A thread elsewhere is refused. A `read_only` chat may
//! only read (search, status).
use super::*;

pub(in crate::thread_tools) const T3_THREAD: &str = "wonder_code_thread";

pub(in crate::thread_tools) fn spec() -> Value {
    json!({"type":"function","name":T3_THREAD,
        "description":"Work with the owner's T3 Code threads, once the owner connected T3 Code in Cupola. action search: find threads by title, branch, pull request or message text (needs query; returns threadId values). The other actions take a threadId from search, or default to the T3 thread this chat continues. action status: its branch, worktree folder and pull requests, and whether it is settled, snoozed or archived. action organize: pin, unpin, snooze (needs snoozedUntil), unsnooze, settle, unsettle, archive, unarchive or mark_unread. action rename: give it a new title. action fork: copy it into a new T3 thread with the same setup; no turn runs there yet. Only threads of T3 projects that use this Project's folders are reachable. Changing a thread (organize, rename, fork) is refused in a read-only chat. Returns a plain error when T3 Code is not connected or has no such thread.",
        "inputSchema":{"type":"object","additionalProperties":false,
            "properties":{
                "action":{"type":"string","enum":["search","status","organize","rename","fork"]},
                "threadId":{"type":"string","maxLength":200,"description":"The T3 thread to act on, from search. Defaults to this chat's own T3 thread."},
                "query":{"type":"string","maxLength":200,"description":"For search."},
                "limit":{"type":"integer","minimum":1,"maximum":50,"description":"For search."},
                "organize":{"type":"string","enum":crate::t3_code::threads::ORGANIZE_ACTIONS,"description":"For action organize."},
                "snoozedUntil":{"type":"string","description":"An ISO 8601 time, for organize snooze."},
                "title":{"type":"string","maxLength":200,"description":"For rename, or the fork's title."}},
            "required":["action"]}})
}

pub(in crate::thread_tools) async fn run(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &[
            "action",
            "threadId",
            "query",
            "limit",
            "organize",
            "snoozedUntil",
            "title",
        ],
    )?;
    let action = text_arg(&args, "action", 20)?;
    let t3 = &state.t3_code;
    if !t3.connected() {
        return Err(ToolError::new(
            "not_connected",
            "T3 Code isn't connected to Cupola. The owner connects it in Settings.",
        ));
    }
    let optional = |name: &str| args.get(name).and_then(Value::as_str);
    if action == "search" {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let query = text_arg(&args, "query", 200)?;
        let (projects, _) = scope(t3, caller).await?;
        let mut merged = Vec::new();
        for project in &projects {
            let found = t3
                .search(&query, Some(project), limit)
                .await
                .map_err(t3_error)?;
            merged.extend(
                found
                    .get("threads")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
        }
        return Ok(json!({"threads": merged}));
    }
    if action != "status" && caller.conversation.access_mode == "read_only" {
        return Err(ToolError::new(
            "read_only",
            "This chat is read-only, so it can't change T3 Code threads. It can search and read their status.",
        ));
    }
    let thread = match optional("threadId")
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) if id.len() > 200 => {
            return Err(ToolError::new(
                "invalid",
                "threadId is longer than 200 bytes.",
            ))
        }
        Some(id) => id.to_owned(),
        None => crate::t3_code::thread_of(state, &caller.conversation)
            .await
            .ok_or_else(|| {
                ToolError::new(
                    "not_in_t3_code",
                    "This chat has no T3 thread. Search for the thread and pass its threadId.",
                )
            })?,
    };
    in_scope(t3, caller, &thread).await?;
    let outcome = match action.as_str() {
        "status" => t3.placement(&thread).await,
        "organize" => {
            let what = text_arg(&args, "organize", 20)?;
            t3.organize(&thread, &what, optional("snoozedUntil"))
                .await
                .map(|_| json!({"threadId": thread, "organized": what}))
        }
        "rename" => {
            let title = text_arg(&args, "title", 800)?;
            t3.rename(&thread, &title)
                .await
                .map(|title| json!({"threadId": thread, "title": title}))
        }
        "fork" => t3
            .fork(&thread, optional("title"), None)
            .await
            .map(|forked| json!({"threadId": forked, "forkedFrom": thread})),
        _ => {
            return Err(ToolError::new(
                "invalid",
                "action must be search, status, organize, rename or fork.",
            ))
        }
    };
    outcome.map_err(t3_error)
}

fn t3_error(error: crate::t3_code::CallError) -> ToolError {
    ToolError::new("t3_code", crate::t3_code::threads::plain(&error))
}

/// The T3 projects that use one of this Project's folders, and those folders.
async fn scope(
    t3: &crate::t3_code::T3Code,
    caller: &Caller,
) -> Result<(Vec<String>, Vec<String>), ToolError> {
    let roots: Vec<String> = caller
        .project
        .roots
        .iter()
        .flat_map(|root| [root.path.clone(), root.canonical_path.clone()])
        .map(|path| path.trim_end_matches('/').to_owned())
        .collect();
    let projects = t3.projects().await.map_err(t3_error)?;
    let ids: Vec<String> = projects
        .into_iter()
        .filter(|(_, folder)| roots.contains(folder))
        .map(|(id, _)| id)
        .collect();
    if ids.is_empty() {
        return Err(ToolError::new(
            "no_t3_project",
            "T3 Code has no project for this Cupola Project's folders, so there are no T3 threads to reach.",
        ));
    }
    Ok((ids, roots))
}

/// Refuses a thread outside this Project's T3 projects and worktrees.
async fn in_scope(
    t3: &crate::t3_code::T3Code,
    caller: &Caller,
    thread: &str,
) -> Result<(), ToolError> {
    let (projects, roots) = scope(t3, caller).await?;
    let read = t3
        .call(
            "t3_thread_read",
            json!({"threadId": thread, "view": "messages", "limit": 1}),
        )
        .await
        .map_err(t3_error)?;
    let detail = read.get("thread").cloned().unwrap_or_default();
    let in_project = detail
        .get("projectId")
        .and_then(Value::as_str)
        .is_some_and(|project| projects.iter().any(|id| id == project));
    let in_worktree = detail
        .get("worktreePath")
        .and_then(Value::as_str)
        .is_some_and(|tree| {
            let tree = tree.trim_end_matches('/');
            roots
                .iter()
                .any(|root| tree == root || tree.starts_with(&format!("{root}/")))
        });
    if in_project || in_worktree {
        Ok(())
    } else {
        Err(ToolError::new(
            "other_project",
            "That T3 thread belongs to another project. An agent can only change T3 threads of this Cupola Project's folders.",
        ))
    }
}
