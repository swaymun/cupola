//! `wonder_schedule`, `wonder_schedules` and `wonder_schedule_update`: an
//! agent sets up a check-back ("in 20 minutes") or a repeating task ("every
//! weekday at 9") as an ordinary Automation the owner sees, pauses and
//! deletes in Automations. They call the same owners as the phone
//! (`save_automation`, `change_automation`, `remove_automation`), so retries,
//! conflicts and target checks are identical.
//!
//! An agent-made automation's ID names the chat that made it. Its runs post
//! into the destination chat and work under that chat's own settings, and a
//! run never starts while the destination has wider access than its maker
//! (`run_authority`).
use super::*;
use crate::automation_schedule::{describe, one_shot_at, one_shot_rule, recurring_rule, Every};
use chrono::{DateTime, TimeZone};
use chrono_tz::Tz;

pub(in crate::thread_tools) const SCHEDULE: &str = "wonder_schedule";
pub(in crate::thread_tools) const SCHEDULES: &str = "wonder_schedules";
pub(in crate::thread_tools) const SCHEDULE_UPDATE: &str = "wonder_schedule_update";

/// Agent-made automations in one Project, active or not.
const PER_PROJECT: usize = 20;
const ID_PREFIX: &str = "agent-automation:";
const PROMPT_BYTES: usize = 8_000;
const MANAGE: &str = "Pause or delete it in Automations.";
const COMPUTER_ON: &str = "It runs only while this computer is on and Cupola is running. A run missed while it was off happens once when it is back.";

/// The chat whose agent made this automation; None when the owner made it.
pub(crate) fn creator(automation_id: &str) -> Option<&str> {
    automation_id
        .strip_prefix(ID_PREFIX)?
        .rsplit_once(':')
        .map(|(conversation, _)| conversation)
        .filter(|conversation| !conversation.is_empty())
}

pub(in crate::thread_tools) fn specs() -> Vec<Value> {
    let object = |properties: Value, required: Value| json!({"type":"object","additionalProperties":false,"properties":properties,"required":required});
    let spec = |name: &str, description: &str, schema: Value| json!({"type":"function","name":name,"description":description,"inputSchema":schema});
    let every = object(
        json!({
            "frequency":{"type":"string","enum":["minutely","hourly","daily","weekly","monthly"]},
            "interval":{"type":"integer","minimum":1,"maximum":1440,"description":"Minutely: every N minutes. Hourly: every N hours, N dividing 24. Omit otherwise."},
            "hour":{"type":"integer","minimum":0,"maximum":23,"description":"Daily, weekly or monthly; default 9."},
            "minute":{"type":"integer","minimum":0,"maximum":59,"description":"Hourly, daily, weekly or monthly; default 0."},
            "weekdays":{"type":"array","items":{"type":"string","enum":["MO","TU","WE","TH","FR","SA","SU"]},"description":"Daily or weekly: the days it runs. Required for weekly."},
            "monthDay":{"type":"integer","minimum":1,"maximum":31,"description":"Monthly: the day of the month; a month without it is skipped."}
        }),
        json!(["frequency"]),
    );
    let when = json!({"type":"object","additionalProperties":false,"minProperties":1,"maxProperties":1,
        "description":"Exactly one of at (one run at that time: RFC 3339, or a local time such as 2026-10-26T09:00 read in timezone), inMinutes (one run that many minutes from now) or every (a repeating schedule).",
        "properties":{"at":{"type":"string"},"inMinutes":{"type":"integer","minimum":1,"maximum":525600},"every":every}});
    let request_id = json!({"type":"string","format":"uuid","description":"A new UUID for this schedule. Reuse it when you retry; never reuse it for a different schedule."});
    vec![
        spec(SCHEDULE, "Schedule a later turn: Cupola sends prompt as a message to this chat (or threadId, another chat of this Project with no wider access than yours) once at a time, or on a repeating schedule. Use it to check back on something (\"check CI in 20 minutes\") or for a routine (\"every weekday at 9, summarise new PRs\"). The prompt is all the later turn gets as its request, so make it self-contained. It shows in the owner's Automations, where they can pause or delete it. Runs need this computer on. Returns message, a sentence to pass on to the reader, and the next run time.",
            object(json!({"name":{"type":"string","maxLength":120,"description":"A short name the owner sees, e.g. Check CI."},"prompt":{"type":"string","maxLength":PROMPT_BYTES},"when":when.clone(),"timezone":{"type":"string","maxLength":80,"description":"An IANA time zone such as Europe/London; use the reader's own when you know it."},"threadId":{"type":"string","description":"Omit for this chat."},"notify":{"type":"string","enum":["all_runs","failed_runs_only"],"description":"Which runs notify the owner; default all_runs."},"clientRequestId":request_id}), json!(["name","prompt","when","timezone","clientRequestId"]))),
        spec(SCHEDULES, "List the automations of this Project's chats: name, schedule, status (active, paused or done), next run, destination chat and whether an agent made it. Read-only.",
            object(json!({}), json!([]))),
        spec(SCHEDULE_UPDATE, "Change, pause, resume or delete an automation an agent made in this Project (from wonder_schedules). Those the owner made can only be changed by them in Automations. status deleted removes it and takes no other field.",
            object(json!({"automationId":{"type":"string"},"status":{"type":"string","enum":["active","paused","deleted"]},"name":{"type":"string","maxLength":120},"prompt":{"type":"string","maxLength":PROMPT_BYTES},"when":when,"timezone":{"type":"string","maxLength":80},"notify":{"type":"string","enum":["all_runs","failed_runs_only"]}}), json!(["automationId"]))),
    ]
}

fn tool_error((status, message): AutomationError) -> ToolError {
    let code = match status {
        StatusCode::BAD_REQUEST => "invalid",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::CONFLICT => "conflict",
        _ => "unavailable",
    };
    ToolError::new(code, message.into_owned())
}

fn zone(name: &str) -> Result<Tz, ToolError> {
    name.parse().map_err(|_| {
        ToolError::new(
            "invalid",
            "timezone must be an IANA time zone such as Europe/London.",
        )
    })
}

/// The rule for `when`, read in `timezone` where it names a local time.
fn rule_for(when: Option<&Value>, timezone: Tz) -> Result<String, ToolError> {
    let invalid = |message: &str| ToolError::new("invalid", message);
    let when = when
        .and_then(Value::as_object)
        .filter(|when| when.len() == 1)
        .ok_or_else(|| invalid("when needs exactly one of at, inMinutes or every."))?;
    let (key, value) = when.iter().next().expect("one field");
    let at = match key.as_str() {
        "at" => {
            let text = value
                .as_str()
                .ok_or_else(|| invalid("at must be a date and time."))?;
            match DateTime::parse_from_rfc3339(text) {
                Ok(at) => at.with_timezone(&Utc),
                Err(_) => ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"]
                    .iter()
                    .find_map(|format| chrono::NaiveDateTime::parse_from_str(text, format).ok())
                    .and_then(|local| timezone.from_local_datetime(&local).earliest())
                    .map(|at| at.with_timezone(&Utc))
                    .ok_or_else(|| {
                        invalid("at must look like 2026-10-26T09:00 or 2026-10-26T09:00:00Z.")
                    })?,
            }
        }
        "inMinutes" => {
            let minutes = value
                .as_i64()
                .filter(|minutes| (1..=525_600).contains(minutes))
                .ok_or_else(|| invalid("inMinutes must be 1 to 525600."))?;
            Utc::now() + chrono::Duration::minutes(minutes)
        }
        "every" => {
            let every = value
                .as_object()
                .ok_or_else(|| invalid("every must be an object."))?;
            const KNOWN: [&str; 6] = [
                "frequency",
                "interval",
                "hour",
                "minute",
                "weekdays",
                "monthDay",
            ];
            if let Some(key) = every.keys().find(|key| !KNOWN.contains(&key.as_str())) {
                return Err(invalid(&format!(
                    "every has no field {key}; use {}.",
                    KNOWN.join(", ")
                )));
            }
            let number = |name: &str| -> Result<Option<u32>, ToolError> {
                match every.get(name) {
                    None | Some(Value::Null) => Ok(None),
                    Some(value) => value
                        .as_u64()
                        .and_then(|value| u32::try_from(value).ok())
                        .map(Some)
                        .ok_or_else(|| invalid(&format!("every.{name} must be a whole number."))),
                }
            };
            let weekdays = match every.get("weekdays") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(days)) => days
                    .iter()
                    .map(|day| {
                        day.as_str()
                            .ok_or_else(|| invalid("weekdays are like MO, TU."))
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err(invalid("weekdays must be a list such as [\"MO\",\"FR\"].")),
            };
            return recurring_rule(&Every {
                frequency: every
                    .get("frequency")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                interval: number("interval")?,
                hour: number("hour")?,
                minute: number("minute")?,
                weekdays,
                month_day: number("monthDay")?,
            })
            .map_err(|error| ToolError::new("invalid", error));
        }
        _ => return Err(invalid("when needs exactly one of at, inMinutes or every.")),
    };
    if at <= Utc::now() {
        return Err(invalid(
            "That time has passed. Choose a time in the future.",
        ));
    }
    Ok(one_shot_rule(at))
}

fn notify_arg(args: &Value) -> Result<Option<String>, ToolError> {
    match args.get("notify") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if matches!(value.as_str(), "all_runs" | "failed_runs_only") => {
            Ok(Some(value.clone()))
        }
        Some(_) => Err(ToolError::new(
            "invalid",
            "notify must be all_runs or failed_runs_only.",
        )),
    }
}

/// done: a single run that already happened. It stays listed until deleted.
fn status(automation: &wonder_store::StoredAutomation) -> &'static str {
    if automation.status == "paused" {
        "paused"
    } else if automation.next_run_at.is_none() && one_shot_at(&automation.rrule).is_some() {
        "done"
    } else {
        "active"
    }
}

fn local(at: Option<&str>, timezone: &str) -> Option<String> {
    let zone: Tz = timezone.parse().ok()?;
    let at = DateTime::parse_from_rfc3339(at?).ok()?;
    Some(
        at.with_timezone(&zone)
            .to_rfc3339_opts(SecondsFormat::Secs, false),
    )
}

fn item(automation: &wonder_store::StoredAutomation, thread: &StoredProjectConversation) -> Value {
    json!({
        "automationId": automation.id, "name": automation.name,
        "schedule": describe(&automation.rrule, &automation.timezone),
        "timezone": automation.timezone, "status": status(automation),
        "nextRunAt": automation.next_run_at,
        "nextRunLocal": local(automation.next_run_at.as_deref(), &automation.timezone),
        "lastRunAt": automation.last_run_at,
        "threadId": thread.conversation_id, "threadTitle": thread.title,
        "notify": automation.notification_policy,
        "createdByAgent": creator(&automation.id).is_some(),
        "createdInThreadId": creator(&automation.id),
        "prompt": clip(&automation.prompt, 2_000),
    })
}

/// "in Release checks" when the run lands in a chat other than the caller's.
fn destination(caller: &Caller, thread: &StoredProjectConversation) -> String {
    if thread.conversation_id == caller.conversation.conversation_id {
        String::new()
    } else {
        format!(" in {}", thread.title)
    }
}

fn when_text(automation: &wonder_store::StoredAutomation) -> String {
    describe(&automation.rrule, &automation.timezone).unwrap_or_else(|| "its schedule".into())
}

pub(in crate::thread_tools) async fn schedule(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &[
            "name",
            "prompt",
            "when",
            "timezone",
            "threadId",
            "notify",
            "clientRequestId",
        ],
    )?;
    forbid_plan_mode(caller)?;
    let request = request_id(&args)?;
    let name = text_arg(&args, "name", 120)?;
    let prompt = text_arg(&args, "prompt", PROMPT_BYTES)?;
    let timezone = text_arg(&args, "timezone", 80)?;
    let mut rrule = rule_for(args.get("when"), zone(&timezone)?)?;
    let notify = notify_arg(&args)?;
    let thread = match args.get("threadId") {
        None | Some(Value::Null) => caller.conversation.clone(),
        Some(_) => target(state, caller, &text_arg(&args, "threadId", 128)?).await?,
    };
    within_authority(&caller.conversation, &thread)?;
    let prefix = format!("{ID_PREFIX}{}", caller.conversation.conversation_id);
    let id = automation_request_id(&prefix, Some(&request))
        .map_err(|error| ToolError::new("invalid", error))?;
    let existing = state.store.automation_by_id(&id).await?;
    // "In 20 minutes" means from the first attempt, so a retry keeps the
    // time that attempt saved instead of differing from it.
    if let Some(existing) = &existing {
        let relative = args["when"].get("inMinutes").is_some();
        if relative && one_shot_at(&existing.rrule).is_some() {
            rrule = existing.rrule.clone();
        }
    }
    // A retry of an accepted schedule is never refused by the limit.
    if existing.is_none() {
        let threads: HashSet<_> = state
            .store
            .project_conversations(&caller.project.id)
            .await?
            .into_iter()
            .map(|thread| thread.conversation_id)
            .collect();
        let made = state
            .store
            .list_automations()
            .await?
            .iter()
            .filter(|automation| {
                creator(&automation.id).is_some() && threads.contains(automation.target.scope_id())
            })
            .count();
        if made >= PER_PROJECT {
            return Err(ToolError::new(
                "limit",
                format!("This Project already has {PER_PROJECT} automations made by agents. Delete one with wonder_schedule_update first."),
            ));
        }
    }
    let (created, automation) = save_automation(
        state,
        CreateAutomationRequest {
            name,
            kind: "continuation".into(),
            conversation_id: Some(thread.conversation_id.clone()),
            prompt,
            rrule,
            timezone,
            status: Some("active".into()),
            notification_policy: notify,
            scope_type: Some("project_thread".into()),
            scope_id: Some(thread.conversation_id.clone()),
            client_request_id: Some(request),
        },
        &prefix,
    )
    .await
    .map_err(tool_error)?;
    let mut result = item(&automation, &thread);
    result["outcome"] = json!(if created == StatusCode::CREATED {
        "created"
    } else {
        "already_scheduled"
    });
    result["note"] = json!(COMPUTER_ON);
    result["message"] = json!(format!(
        "Scheduled {} for {}{}. {MANAGE}",
        automation.name,
        when_text(&automation),
        destination(caller, &thread)
    ));
    Ok(result)
}

pub(in crate::thread_tools) async fn schedules(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(&args, &[])?;
    let threads: HashMap<_, _> = state
        .store
        .project_conversations(&caller.project.id)
        .await?
        .into_iter()
        .map(|thread| (thread.conversation_id.clone(), thread))
        .collect();
    let items: Vec<_> = state
        .store
        .list_automations()
        .await?
        .iter()
        .filter_map(|automation| {
            threads
                .get(automation.target.scope_id())
                .map(|thread| item(automation, thread))
        })
        .collect();
    Ok(json!({"automations": items}))
}

pub(in crate::thread_tools) async fn update(
    state: &AppState,
    caller: &Caller,
    args: Value,
) -> Result<Value, ToolError> {
    only_known(
        &args,
        &[
            "automationId",
            "status",
            "name",
            "prompt",
            "when",
            "timezone",
            "notify",
        ],
    )?;
    let id = text_arg(&args, "automationId", 512)?;
    let missing = || ToolError::new("not_found", "No such automation in this Project.");
    let automation = state
        .store
        .automation_by_id(&id)
        .await?
        .ok_or_else(missing)?;
    let thread = target(state, caller, automation.target.scope_id())
        .await
        .map_err(|_| missing())?;
    if creator(&automation.id).is_none() {
        return Err(ToolError::new(
            "owner_automation",
            "The owner made this automation; only they can change it, in Automations.",
        ));
    }
    within_authority(&caller.conversation, &thread)?;
    let status = match args.get("status") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if matches!(value.as_str(), "active" | "paused" | "deleted") => {
            Some(value.as_str())
        }
        Some(_) => {
            return Err(ToolError::new(
                "invalid",
                "status must be active, paused or deleted.",
            ))
        }
    };
    if status == Some("deleted") {
        if args.as_object().is_some_and(|object| object.len() > 2) {
            return Err(ToolError::new(
                "invalid",
                "Deleting takes only automationId and status.",
            ));
        }
        remove_automation(state, &id).await.map_err(tool_error)?;
        return Ok(json!({"automationId": id, "status": "deleted",
            "message": format!("Deleted {}.", automation.name)}));
    }
    // Pausing is always allowed; anything that can start work is not while planning.
    if status != Some("paused") || args.as_object().is_some_and(|object| object.len() > 2) {
        forbid_plan_mode(caller)?;
    }
    let timezone = match args.get("timezone") {
        None | Some(Value::Null) => None,
        Some(_) => Some(text_arg(&args, "timezone", 80)?),
    };
    let zone = zone(timezone.as_deref().unwrap_or(&automation.timezone))?;
    let rrule = match args.get("when") {
        None | Some(Value::Null) => None,
        when => Some(rule_for(when, zone)?),
    };
    let text = |name: &str, max: usize| match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => text_arg(&args, name, max).map(Some),
    };
    let final_rule = rrule.as_deref().unwrap_or(&automation.rrule);
    let final_status = status.unwrap_or(&automation.status);
    if final_status == "active" && one_shot_at(final_rule).is_some_and(|at| at <= Utc::now()) {
        return Err(ToolError::new(
            "invalid",
            "That single run already happened. Give it a new time with when, or delete it.",
        ));
    }
    let updated = change_automation(
        state,
        &id,
        UpdateAutomationRequest {
            expected_revision: Some(automation.revision),
            name: text("name", 120)?,
            prompt: text("prompt", PROMPT_BYTES)?,
            rrule,
            timezone,
            status: status.map(str::to_owned),
            notification_policy: notify_arg(&args)?,
            ..Default::default()
        },
    )
    .await
    .map_err(tool_error)?;
    let mut result = item(&updated, &thread);
    result["message"] = json!(if updated.status == "paused" {
        format!(
            "Paused {}. Resume or delete it in Automations.",
            updated.name
        )
    } else {
        format!(
            "{} {} for {}{}. {MANAGE}",
            if status == Some("active") && automation.status == "paused" {
                "Resumed"
            } else {
                "Updated"
            },
            updated.name,
            when_text(&updated),
            destination(caller, &thread)
        )
    });
    Ok(result)
}

/// Whether a run of `automation` may start now: an agent's schedule for
/// another chat needs its maker to still exist in the same Project with at
/// least the destination's access, as it did when it was made.
pub(crate) async fn run_authority(
    state: &AppState,
    automation: &wonder_store::StoredAutomation,
) -> Result<(), &'static str> {
    let Some(maker) = creator(&automation.id) else {
        return Ok(());
    };
    let destination = automation.target.scope_id();
    if maker == destination {
        return Ok(());
    }
    let unavailable =
        "Cupola could not check this automation's access. It tries again at the next run.";
    let from = state
        .store
        .project_conversation(maker)
        .await
        .map_err(|_| unavailable)?;
    let to = state
        .store
        .project_conversation(destination)
        .await
        .map_err(|_| unavailable)?;
    match (from, to) {
        (Some(from), Some(to))
            if from.project_id == to.project_id && within_authority(&from, &to).is_ok() =>
        {
            Ok(())
        }
        (None, _) => Err("The chat whose agent scheduled this was removed, so it no longer runs. Delete it, or schedule it again from a chat with this access."),
        _ => Err("This chat now has wider access than the chat whose agent scheduled it, so it did not run. Schedule it again from a chat with the same access, or delete it."),
    }
}
