//! Deliberately bounded recurrence support, shared by preview and execution.
//! Besides the repeating subset, `DTSTART=<UTC>;COUNT=1` is a single run at
//! that instant: it has no next occurrence once that time has passed.
use chrono::{
    DateTime, Datelike, Duration, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc, Weekday,
};
use chrono_tz::Tz;
use std::collections::HashSet;

const ONE_SHOT_FORMAT: &str = "%Y%m%dT%H%M%SZ";
const DAYS: [(&str, Weekday, &str); 7] = [
    ("MO", Weekday::Mon, "Monday"),
    ("TU", Weekday::Tue, "Tuesday"),
    ("WE", Weekday::Wed, "Wednesday"),
    ("TH", Weekday::Thu, "Thursday"),
    ("FR", Weekday::Fri, "Friday"),
    ("SA", Weekday::Sat, "Saturday"),
    ("SU", Weekday::Sun, "Sunday"),
];

/// The rule for a single run at `at`, kept to the second.
pub(crate) fn one_shot_rule(at: DateTime<Utc>) -> String {
    format!("DTSTART={};COUNT=1", at.format(ONE_SHOT_FORMAT))
}

/// When a single-run rule runs; None for a repeating rule.
pub(crate) fn one_shot_at(rule: &str) -> Option<DateTime<Utc>> {
    let start = rule.strip_suffix(";COUNT=1")?.strip_prefix("DTSTART=")?;
    NaiveDateTime::parse_from_str(start, ONE_SHOT_FORMAT)
        .ok()
        .map(|value| value.and_utc())
}

/// A repeating schedule as an agent describes it, before it becomes a rule.
#[derive(Debug, Default)]
pub(crate) struct Every<'a> {
    pub frequency: &'a str,
    pub interval: Option<u32>,
    pub hour: Option<u32>,
    pub minute: Option<u32>,
    pub weekdays: Vec<&'a str>,
    pub month_day: Option<u32>,
}

/// The supported rule for `every`. The parser still validates the result,
/// so its errors stay the only statement of what is supported.
pub(crate) fn recurring_rule(every: &Every) -> Result<String, String> {
    let mut days = Vec::new();
    for day in &every.weekdays {
        let code = DAYS
            .iter()
            .find(|(code, _, name)| {
                day.eq_ignore_ascii_case(code) || day.eq_ignore_ascii_case(name)
            })
            .map(|(code, _, _)| *code)
            .ok_or_else(|| format!("{day} is not a weekday."))?;
        if !days.contains(&code) {
            days.push(code);
        }
    }
    // Keep the week's order, which is also how the phone's editor writes it.
    days.sort_by_key(|code| DAYS.iter().position(|(day, _, _)| day == code));
    let mut fields = Vec::new();
    match every.frequency {
        "minutely" | "hourly" => {
            fields.push(format!("FREQ={}", every.frequency.to_uppercase()));
            if let Some(interval) = every.interval.filter(|value| *value != 1) {
                fields.push(format!("INTERVAL={interval}"));
            }
        }
        // Daily on chosen weekdays is the phone editor's weekly rule.
        "daily" | "weekly" if !days.is_empty() => {
            fields.push("FREQ=WEEKLY".into());
            fields.push(format!("BYDAY={}", days.join(",")));
        }
        "daily" | "monthly" => fields.push(format!("FREQ={}", every.frequency.to_uppercase())),
        "weekly" => return Err("Choose the weekdays for a weekly schedule.".into()),
        _ => return Err("Choose minutely, hourly, daily, weekly or monthly.".into()),
    }
    if matches!(every.frequency, "daily" | "weekly" | "monthly") {
        if every.interval.is_some_and(|value| value != 1) {
            return Err("This interval is not supported for the chosen frequency.".into());
        }
        fields.push(format!("BYHOUR={}", every.hour.unwrap_or(9)));
    }
    if every.frequency != "minutely" {
        fields.push(format!("BYMINUTE={}", every.minute.unwrap_or(0)));
    }
    if every.frequency == "monthly" {
        fields.push(format!("BYMONTHDAY={}", every.month_day.unwrap_or(1)));
    } else if every.month_day.is_some() {
        return Err("monthDay applies only to a monthly schedule.".into());
    }
    if !matches!(every.frequency, "daily" | "weekly") && !days.is_empty() {
        return Err("weekdays apply only to a daily or weekly schedule.".into());
    }
    if every.frequency == "minutely" && (every.hour.is_some() || every.minute.is_some()) {
        return Err("A schedule every few minutes has no hour or minute.".into());
    }
    if every.frequency == "hourly" && every.hour.is_some() {
        return Err("An hourly schedule has a minute, not an hour.".into());
    }
    let rule = fields.join(";");
    next_automation_run(&rule, "UTC", Utc::now())?;
    Ok(rule)
}

/// When a schedule runs, in words that follow "for": "9:00 every weekday",
/// "14:20 on Sun 25 Oct 2026", "every 30 minutes". None for a rule this
/// module does not accept.
pub(crate) fn describe(rule: &str, zone: &str) -> Option<String> {
    let timezone = zone.parse::<Tz>().ok()?;
    if let Some(at) = one_shot_at(rule) {
        return Some(
            at.with_timezone(&timezone)
                .format("%-H:%M on %a %-d %b %Y")
                .to_string(),
        );
    }
    next_automation_run(rule, zone, Utc::now()).ok()?;
    let fields: std::collections::HashMap<_, _> = rule
        .split(';')
        .filter_map(|field| field.split_once('='))
        .collect();
    let number = |key: &str, default: u32| {
        fields
            .get(key)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(default)
    };
    let interval = number("INTERVAL", 1);
    let time = format!("{}:{:02}", number("BYHOUR", 9), number("BYMINUTE", 0));
    Some(match *fields.get("FREQ")? {
        "MINUTELY" if interval == 1 => "every minute".into(),
        "MINUTELY" => format!("every {interval} minutes"),
        "HOURLY" if interval == 1 => format!("every hour at :{:02}", number("BYMINUTE", 0)),
        "HOURLY" => format!("every {interval} hours at :{:02}", number("BYMINUTE", 0)),
        "MONTHLY" => format!("{time} on day {} of every month", number("BYMONTHDAY", 1)),
        _ => match fields.get("BYDAY").copied() {
            None => format!("{time} every day"),
            Some("MO,TU,WE,TH,FR") => format!("{time} every weekday"),
            Some("SA,SU") => format!("{time} every weekend day"),
            Some(days) => {
                let names: Vec<_> = days
                    .split(',')
                    .filter_map(|day| DAYS.iter().find(|(code, _, _)| *code == day))
                    .map(|(_, _, name)| *name)
                    .collect();
                match names.split_last() {
                    Some((last, [])) => format!("{time} every {last}"),
                    Some((last, rest)) => format!("{time} every {} and {last}", rest.join(", ")),
                    None => return None,
                }
            }
        },
    })
}

pub(crate) fn next_automation_run(
    rule: &str,
    zone: &str,
    after: DateTime<Utc>,
) -> Result<Option<String>, String> {
    let timezone = zone
        .parse::<Tz>()
        .map_err(|_| "Choose a valid timezone.".to_owned())?;
    let mut frequency = "";
    let mut interval = 1u32;
    let mut hour = 9u32;
    let mut minute = 0u32;
    let mut month_day = 1u32;
    let mut weekdays = Vec::new();
    let mut seen = HashSet::new();
    for field in rule.split(';') {
        let (key, value) = field
            .split_once('=')
            .ok_or("The schedule rule is malformed.")?;
        if !seen.insert(key) {
            return Err("The schedule repeats a field.".into());
        }
        match key {
            "FREQ"
                if matches!(
                    value,
                    "MINUTELY" | "HOURLY" | "DAILY" | "WEEKLY" | "MONTHLY"
                ) =>
            {
                frequency = value
            }
            "INTERVAL" => interval = value.parse().map_err(|_| "The interval is invalid.")?,
            "BYHOUR" => hour = value.parse().map_err(|_| "The hour is invalid.")?,
            "BYMINUTE" => minute = value.parse().map_err(|_| "The minute is invalid.")?,
            "BYMONTHDAY" => {
                month_day = value.parse().map_err(|_| "The day of month is invalid.")?
            }
            "BYDAY" => {
                for day in value.split(',') {
                    weekdays.push(
                        DAYS.iter()
                            .find(|(code, _, _)| *code == day)
                            .map(|(_, weekday, _)| *weekday)
                            .ok_or("The weekday is invalid.")?,
                    );
                }
            }
            "DTSTART" | "COUNT" => {}
            _ => return Err("The schedule contains an unsupported field.".into()),
        }
    }
    let format = |value: DateTime<Utc>| Some(value.to_rfc3339_opts(SecondsFormat::Millis, true));
    if seen.contains("DTSTART") || seen.contains("COUNT") {
        // A single run is exactly these two fields; nothing repeats it.
        let at = one_shot_at(rule).filter(|_| seen.len() == 2).ok_or(
            "A single run needs only its start time, as DTSTART=YYYYMMDDTHHMMSSZ;COUNT=1.",
        )?;
        return Ok((at > after).then(|| format(at)).flatten());
    }
    if frequency.is_empty()
        || hour > 23
        || minute > 59
        || !(1..=31).contains(&month_day)
        || !(1..=1440).contains(&interval)
    {
        return Err("The schedule contains an invalid value.".into());
    }
    if (!matches!(frequency, "MINUTELY" | "HOURLY") && interval != 1)
        || (frequency == "HOURLY" && (interval > 24 || 24 % interval != 0))
    {
        return Err("This interval is not supported for the chosen frequency.".into());
    }
    if (frequency == "MINUTELY" && seen.iter().any(|key| key.starts_with("BY")))
        || (frequency == "HOURLY"
            && (seen.contains("BYHOUR") || seen.contains("BYMONTHDAY") || seen.contains("BYDAY")))
        || (frequency != "MONTHLY" && seen.contains("BYMONTHDAY"))
        || (frequency == "MONTHLY" && seen.contains("BYDAY"))
        || (frequency == "WEEKLY" && weekdays.is_empty())
    {
        return Err("This schedule combination is not supported.".into());
    }
    if frequency == "MINUTELY" {
        // Absolute minute boundaries remain continuous across DST changes and
        // correctly support intervals greater than one hour.
        let minutes = after.timestamp().div_euclid(60);
        let next = (minutes.div_euclid(i64::from(interval)) + 1) * i64::from(interval) * 60;
        return Ok(format(
            DateTime::from_timestamp(next, 0).ok_or("Schedule is out of range.")?,
        ));
    }
    if frequency == "HOURLY" {
        let mut candidate = after
            .with_second(0)
            .and_then(|v| v.with_nanosecond(0))
            .ok_or("Schedule is out of range.")?
            + Duration::minutes(1);
        for _ in 0..=60 * 49 {
            let local = candidate.with_timezone(&timezone);
            if local.minute() == minute && local.hour().is_multiple_of(interval) {
                return Ok(format(candidate));
            }
            candidate += Duration::minutes(1);
        }
        return Err("No upcoming occurrence was found.".into());
    }
    let date = after.with_timezone(&timezone).date_naive();
    for offset in 0..=370 {
        let date = date + Duration::days(offset);
        if (!weekdays.is_empty() && !weekdays.contains(&date.weekday()))
            || (frequency == "MONTHLY" && date.day() != month_day)
        {
            continue;
        }
        let local = date
            .and_hms_opt(hour, minute, 0)
            .ok_or("Schedule is out of range.")?;
        // Skip nonexistent spring times. A repeated fall time runs once, at
        // its first occurrence, matching the preview shown to the user.
        if let Some(candidate) = timezone.from_local_datetime(&local).earliest() {
            let candidate = candidate.with_timezone(&Utc);
            if candidate > after {
                return Ok(format(candidate));
            }
        }
    }
    Err("No upcoming occurrence was found.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn next(rule: &str, zone: &str, now: &str) -> String {
        next_automation_run(rule, zone, now.parse().unwrap())
            .unwrap()
            .unwrap()
    }
    #[test]
    fn monthly_skips_missing_dates() {
        assert_eq!(
            next(
                "FREQ=MONTHLY;BYMONTHDAY=31;BYHOUR=9",
                "UTC",
                "2026-04-01T00:00:00Z"
            ),
            "2026-05-31T09:00:00.000Z"
        );
    }
    #[test]
    fn weekdays_and_dst_are_explicit() {
        assert_eq!(
            next(
                "FREQ=DAILY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9",
                "America/New_York",
                "2026-03-06T15:00:00Z"
            ),
            "2026-03-09T13:00:00.000Z"
        );
        assert_eq!(
            next(
                "FREQ=DAILY;BYHOUR=2;BYMINUTE=30",
                "America/New_York",
                "2026-03-08T06:00:00Z"
            ),
            "2026-03-09T06:30:00.000Z"
        );
        assert_eq!(
            next(
                "FREQ=DAILY;BYHOUR=1;BYMINUTE=30",
                "America/New_York",
                "2026-11-01T05:31:00Z"
            ),
            "2026-11-02T06:30:00.000Z"
        );
    }
    #[test]
    fn elapsed_intervals_survive_dst() {
        assert_eq!(
            next("FREQ=MINUTELY;INTERVAL=90", "UTC", "2026-01-01T01:31:00Z"),
            "2026-01-01T03:00:00.000Z"
        );
        assert_eq!(
            next(
                "FREQ=HOURLY;BYMINUTE=30",
                "America/New_York",
                "2026-03-08T06:31:00Z"
            ),
            "2026-03-08T07:30:00.000Z"
        );
    }
    #[test]
    fn unsupported_fields_never_silently_disappear() {
        for rule in [
            "FREQ=DAILY;FREQ=WEEKLY",
            "FREQ=MONTHLY;BYDAY=MO",
            "FREQ=MINUTELY;BYHOUR=9",
            "FREQ=HOURLY;INTERVAL=5",
        ] {
            assert!(
                next_automation_run(rule, "UTC", Utc::now()).is_err(),
                "{rule}"
            );
        }
    }

    #[test]
    fn a_single_run_happens_once_at_its_instant() {
        let at: DateTime<Utc> = "2026-10-25T14:20:05Z".parse().unwrap();
        let rule = one_shot_rule(at);
        assert_eq!(rule, "DTSTART=20261025T142005Z;COUNT=1");
        assert_eq!(one_shot_at(&rule), Some(at));
        assert_eq!(
            next(&rule, "Europe/London", "2026-10-25T14:00:00Z"),
            "2026-10-25T14:20:05.000Z"
        );
        // At or after its time there is no next run, so the scheduler's
        // claim leaves it with none.
        for after in ["2026-10-25T14:20:05Z", "2026-10-26T00:00:00Z"] {
            assert_eq!(
                next_automation_run(&rule, "UTC", after.parse().unwrap()),
                Ok(None)
            );
        }
        assert_eq!(
            describe(&rule, "America/New_York").as_deref(),
            Some("10:20 on Sun 25 Oct 2026")
        );
        for rule in [
            "DTSTART=20261025T142005Z",
            "COUNT=1",
            "DTSTART=20261025T142005Z;COUNT=2",
            "FREQ=DAILY;DTSTART=20261025T142005Z;COUNT=1",
            "DTSTART=2026-10-25;COUNT=1",
        ] {
            assert!(
                next_automation_run(rule, "UTC", Utc::now()).is_err(),
                "{rule}"
            );
        }
    }

    #[test]
    fn every_becomes_the_supported_rule_and_reads_plainly() {
        let every = |frequency, weekdays: Vec<&'static str>| Every {
            frequency,
            weekdays,
            ..Default::default()
        };
        let cases = [
            (
                every("daily", vec!["fr", "Monday", "TU", "WE", "TH"]),
                "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR;BYHOUR=9;BYMINUTE=0",
                "9:00 every weekday",
            ),
            (
                Every {
                    hour: Some(7),
                    minute: Some(30),
                    ..every("daily", vec![])
                },
                "FREQ=DAILY;BYHOUR=7;BYMINUTE=30",
                "7:30 every day",
            ),
            (
                every("weekly", vec!["WE", "MO"]),
                "FREQ=WEEKLY;BYDAY=MO,WE;BYHOUR=9;BYMINUTE=0",
                "9:00 every Monday and Wednesday",
            ),
            (
                Every {
                    interval: Some(20),
                    ..every("minutely", vec![])
                },
                "FREQ=MINUTELY;INTERVAL=20",
                "every 20 minutes",
            ),
            (
                Every {
                    interval: Some(6),
                    minute: Some(5),
                    ..every("hourly", vec![])
                },
                "FREQ=HOURLY;INTERVAL=6;BYMINUTE=5",
                "every 6 hours at :05",
            ),
            (
                Every {
                    month_day: Some(15),
                    ..every("monthly", vec![])
                },
                "FREQ=MONTHLY;BYHOUR=9;BYMINUTE=0;BYMONTHDAY=15",
                "9:00 on day 15 of every month",
            ),
        ];
        for (every, rule, words) in cases {
            assert_eq!(recurring_rule(&every).as_deref(), Ok(rule));
            assert_eq!(describe(rule, "UTC").as_deref(), Some(words));
        }
        for wrong in [
            every("weekly", vec![]),
            every("yearly", vec![]),
            every("monthly", vec!["MO"]),
            every("daily", vec!["XX"]),
            Every {
                interval: Some(5),
                ..every("hourly", vec![])
            },
            Every {
                interval: Some(2),
                ..every("daily", vec![])
            },
            Every {
                hour: Some(9),
                ..every("minutely", vec![])
            },
            Every {
                hour: Some(24),
                ..every("daily", vec![])
            },
        ] {
            assert!(recurring_rule(&wrong).is_err(), "{wrong:?}");
        }
    }
}
