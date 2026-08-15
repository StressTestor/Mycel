use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{OrchestrationError, OrchestrationPorts};

const CRON_SCOPE: &str = "cron";
const MINUTE_MS: u64 = 60_000;
const STALE_AFTER_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const SEARCH_WINDOW_MINUTES: u64 = 5 * 366 * 24 * 60;
const MAX_COALESCED: u32 = 10_000;
const MAX_TASKS: usize = 64;
const MAX_PROMPT_CHARS: usize = 100_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronTask {
    pub id: String,
    pub expression: String,
    pub prompt: String,
    pub recurring: bool,
    pub created_at_ms: u64,
    pub last_fired_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronState {
    pub tasks: Vec<CronTask>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronFire {
    pub task_id: String,
    pub prompt: String,
    pub scheduled_at_ms: u64,
    pub fired_at_ms: u64,
    pub coalesced_count: u32,
    pub stale: bool,
}

/// Pure, session-scoped cron reducer. A host owns wakeups and calls `tick`;
/// the reducer deliberately has no hidden timer, environment kill switch, or
/// debug-only forcing path.
pub struct CronScheduler {
    ports: OrchestrationPorts,
    state: Mutex<CronState>,
}

impl CronScheduler {
    pub fn open(ports: OrchestrationPorts) -> Result<Self, CronError> {
        let state: CronState = ports.restore(CRON_SCOPE)?;
        validate_restored_state(&state)?;
        Ok(Self {
            ports,
            state: Mutex::new(state),
        })
    }

    pub fn snapshot(&self) -> CronState {
        lock(&self.state).clone()
    }

    pub fn schedule(
        &self,
        id: &str,
        expression: &str,
        prompt: &str,
        recurring: bool,
    ) -> Result<CronTask, CronError> {
        validate_id(id)?;
        let prompt = normalize_prompt(prompt)?;
        let parsed = CronExpression::parse(expression)?;
        let now = self.ports.now_ms();
        if parsed.next_after(now).is_none() {
            return Err(CronError::NoFireWithinWindow);
        }

        let mut state = lock(&self.state);
        if state.tasks.len() >= MAX_TASKS {
            return Err(CronError::TaskLimit);
        }
        if state.tasks.iter().any(|task| task.id == id) {
            return Err(CronError::DuplicateId(id.to_owned()));
        }
        let task = CronTask {
            id: id.to_owned(),
            expression: parsed.raw,
            prompt,
            recurring,
            created_at_ms: now,
            last_fired_at_ms: None,
        };
        let mut next = state.clone();
        next.tasks.push(task.clone());
        let event = self.ports.persist(
            CRON_SCOPE,
            "scheduled",
            Some(id),
            &next,
            json!({"expression": task.expression, "recurring": recurring}),
        )?;
        *state = next;
        self.ports.publish(event);
        Ok(task)
    }

    pub fn remove(&self, id: &str) -> Result<bool, CronError> {
        validate_id(id)?;
        let mut state = lock(&self.state);
        let mut next = state.clone();
        let before = next.tasks.len();
        next.tasks.retain(|task| task.id != id);
        if next.tasks.len() == before {
            return Ok(false);
        }
        let event = self
            .ports
            .persist(CRON_SCOPE, "removed", Some(id), &next, json!({}))?;
        *state = next;
        self.ports.publish(event);
        Ok(true)
    }

    /// Delivers due work only while the host reports the session idle. A busy
    /// tick does not advance any cursor, so the next idle tick coalesces every
    /// missed ideal occurrence.
    pub fn tick(&self, idle: bool) -> Result<Vec<CronFire>, CronError> {
        if !idle {
            return Ok(Vec::new());
        }
        let now = self.ports.now_ms();
        let mut state = lock(&self.state);
        let mut next = state.clone();
        let mut fires = Vec::new();
        let mut remove_ids = BTreeSet::new();

        for task in &mut next.tasks {
            let parsed = CronExpression::parse(&task.expression)?;
            let baseline = task
                .last_fired_at_ms
                .filter(|cursor| *cursor <= now && *cursor > task.created_at_ms)
                .unwrap_or(task.created_at_ms);
            let Some(first_due) = parsed.next_after(baseline) else {
                continue;
            };
            if first_due > now {
                continue;
            }

            let stale = task.recurring && now.saturating_sub(task.created_at_ms) >= STALE_AFTER_MS;
            let (coalesced_count, last_due) = if task.recurring {
                coalesced_due(&parsed, first_due, now)
            } else {
                (1, first_due)
            };
            fires.push(CronFire {
                task_id: task.id.clone(),
                prompt: task.prompt.clone(),
                scheduled_at_ms: last_due,
                fired_at_ms: now,
                coalesced_count,
                stale,
            });

            if !task.recurring || stale {
                remove_ids.insert(task.id.clone());
            } else {
                task.last_fired_at_ms = Some(last_due);
            }
        }

        if fires.is_empty() {
            return Ok(fires);
        }
        next.tasks
            .retain(|task| !remove_ids.contains(task.id.as_str()));
        let event =
            self.ports
                .persist(CRON_SCOPE, "fired", None, &next, json!({"fires": fires}))?;
        *state = next;
        self.ports.publish(event);
        Ok(fires)
    }

    pub fn next_fire_at_ms(&self, task_id: &str) -> Result<Option<u64>, CronError> {
        let state = lock(&self.state);
        let task = state
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or_else(|| CronError::NotFound(task_id.to_owned()))?;
        let now = self.ports.now_ms();
        let baseline = task
            .last_fired_at_ms
            .filter(|cursor| *cursor <= now && *cursor > task.created_at_ms)
            .unwrap_or(task.created_at_ms);
        CronExpression::parse(&task.expression).map(|parsed| parsed.next_after(baseline))
    }
}

#[derive(Clone, Debug)]
struct CronExpression {
    raw: String,
    minutes: Field,
    hours: Field,
    days_of_month: Field,
    months: Field,
    days_of_week: Field,
}

impl CronExpression {
    fn parse(input: &str) -> Result<Self, CronError> {
        let fields = input.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(CronError::InvalidExpression(
                "expected five fields: minute hour day-of-month month day-of-week".to_owned(),
            ));
        }
        Ok(Self {
            raw: fields.join(" "),
            minutes: Field::parse(fields[0], 0, 59, false)?,
            hours: Field::parse(fields[1], 0, 23, false)?,
            days_of_month: Field::parse(fields[2], 1, 31, false)?,
            months: Field::parse(fields[3], 1, 12, false)?,
            days_of_week: Field::parse(fields[4], 0, 7, true)?,
        })
    }

    fn next_after(&self, from_ms: u64) -> Option<u64> {
        let start_minute = from_ms / MINUTE_MS + 1;
        let end_minute = start_minute.saturating_add(SEARCH_WINDOW_MINUTES);
        for epoch_minute in start_minute..=end_minute {
            if self.matches(epoch_minute) {
                return epoch_minute.checked_mul(MINUTE_MS);
            }
        }
        None
    }

    fn matches(&self, epoch_minute: u64) -> bool {
        let minute_of_day = epoch_minute % (24 * 60);
        let minute = (minute_of_day % 60) as u8;
        let hour = (minute_of_day / 60) as u8;
        let days_since_epoch = (epoch_minute / (24 * 60)) as i64;
        let date = civil_from_days(days_since_epoch);
        if !self.minutes.values.contains(&minute)
            || !self.hours.values.contains(&hour)
            || !self.months.values.contains(&date.month)
        {
            return false;
        }

        let day_of_week = (days_since_epoch + 4).rem_euclid(7) as u8;
        let dom_matches = self.days_of_month.values.contains(&date.day);
        let dow_matches = self.days_of_week.values.contains(&day_of_week);
        match (self.days_of_month.wildcard, self.days_of_week.wildcard) {
            (true, true) => true,
            (true, false) => dow_matches,
            (false, true) => dom_matches,
            (false, false) => dom_matches || dow_matches,
        }
    }
}

#[derive(Clone, Debug)]
struct Field {
    values: BTreeSet<u8>,
    wildcard: bool,
}

impl Field {
    fn parse(input: &str, min: u8, max: u8, sunday_alias: bool) -> Result<Self, CronError> {
        if input.is_empty() {
            return Err(invalid_expression("empty field"));
        }
        let mut values = BTreeSet::new();
        for term in input.split(',') {
            if term.is_empty() {
                return Err(invalid_expression("empty list term"));
            }
            add_term(&mut values, term, min, max)?;
        }
        if sunday_alias && values.remove(&7) {
            values.insert(0);
        }
        if values.is_empty() {
            return Err(invalid_expression("field has no values"));
        }
        Ok(Self {
            values,
            wildcard: input == "*",
        })
    }
}

fn add_term(values: &mut BTreeSet<u8>, term: &str, min: u8, max: u8) -> Result<(), CronError> {
    let mut pieces = term.split('/');
    let range = pieces.next().expect("split always returns one item");
    let step = pieces.next().map(parse_number).transpose()?.unwrap_or(1);
    if pieces.next().is_some() || step == 0 {
        return Err(invalid_expression("invalid step"));
    }

    let (start, end) = if range == "*" {
        (min, max)
    } else if let Some((start, end)) = range.split_once('-') {
        (parse_number(start)?, parse_number(end)?)
    } else {
        let start = parse_number(range)?;
        if term.contains('/') {
            (start, max)
        } else {
            if !(min..=max).contains(&start) {
                return Err(invalid_expression("value out of range"));
            }
            values.insert(start);
            return Ok(());
        }
    };
    if start < min || end > max || start > end {
        return Err(invalid_expression("range out of bounds"));
    }
    let mut value = start;
    while value <= end {
        values.insert(value);
        let Some(next) = value.checked_add(step) else {
            break;
        };
        value = next;
    }
    Ok(())
}

fn parse_number(input: &str) -> Result<u8, CronError> {
    if input.is_empty() || !input.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_expression("expected digits"));
    }
    input
        .parse()
        .map_err(|_| invalid_expression("number is too large"))
}

fn coalesced_due(expression: &CronExpression, first_due: u64, now: u64) -> (u32, u64) {
    let mut count = 1;
    let mut last_due = first_due;
    while count < MAX_COALESCED {
        let Some(next_due) = expression.next_after(last_due) else {
            break;
        };
        if next_due > now {
            break;
        }
        count += 1;
        last_due = next_due;
    }
    (count, last_due)
}

#[derive(Clone, Copy)]
struct CivilDate {
    month: u8,
    day: u8,
}

// Howard Hinnant's civil-from-days transform, with day zero at
// 1970-01-01. Cron evaluation uses UTC so replay does not depend on the host's
// ambient timezone; a local-time adapter belongs at the CLI boundary.
fn civil_from_days(days_since_epoch: i64) -> CivilDate {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    CivilDate {
        month: month as u8,
        day: day as u8,
    }
}

fn validate_restored_state(state: &CronState) -> Result<(), CronError> {
    if state.tasks.len() > MAX_TASKS {
        return Err(CronError::InvalidRestoredState(
            "task count exceeds the session limit".to_owned(),
        ));
    }
    let mut ids = BTreeSet::new();
    for task in &state.tasks {
        validate_id(&task.id)
            .map_err(|error| CronError::InvalidRestoredState(error.to_string()))?;
        normalize_prompt(&task.prompt)
            .map_err(|error| CronError::InvalidRestoredState(error.to_string()))?;
        CronExpression::parse(&task.expression)
            .map_err(|error| CronError::InvalidRestoredState(error.to_string()))?;
        if !ids.insert(&task.id) {
            return Err(CronError::InvalidRestoredState(format!(
                "duplicate task id {:?}",
                task.id
            )));
        }
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), CronError> {
    if id.len() == 8
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(CronError::InvalidId(id.to_owned()))
    }
}

fn normalize_prompt(prompt: &str) -> Result<String, CronError> {
    let prompt = prompt.trim();
    if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS {
        Err(CronError::InvalidPrompt)
    } else {
        Ok(prompt.to_owned())
    }
}

fn invalid_expression(message: &str) -> CronError {
    CronError::InvalidExpression(message.to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("cron task id {0:?} must be eight lowercase hexadecimal characters")]
    InvalidId(String),
    #[error("cron task id {0:?} already exists")]
    DuplicateId(String),
    #[error("cron task {0:?} does not exist")]
    NotFound(String),
    #[error("cron task limit reached")]
    TaskLimit,
    #[error("cron prompt is empty or too large")]
    InvalidPrompt,
    #[error("invalid cron expression: {0}")]
    InvalidExpression(String),
    #[error("cron expression has no fire within five years")]
    NoFireWithinWindow,
    #[error("invalid restored cron state: {0}")]
    InvalidRestoredState(String),
    #[error(transparent)]
    Orchestration(#[from] OrchestrationError),
}

#[cfg(test)]
mod tests {
    use super::CronExpression;

    #[test]
    fn cron_fields_support_lists_ranges_steps_sunday_alias_and_dom_dow_or() {
        let expression = CronExpression::parse("*/15 0-2 1 1 7").expect("parse");
        assert_eq!(expression.next_after(0), Some(900_000));

        // 1970-01-04 is Sunday. The day-of-month does not match, but
        // restricted DOM/DOW fields use cron's OR rule.
        let sunday = CronExpression::parse("0 0 1 1 0").expect("parse");
        assert_eq!(sunday.next_after(0), Some(3 * 24 * 60 * 60 * 1_000));
    }
}
