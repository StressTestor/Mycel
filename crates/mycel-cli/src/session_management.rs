use std::{
    io::{self, Write},
    path::Path,
};

use mycel_agent_protocol::SessionSummary;

/// Injectable startup picker. Returning `None` cancels startup without
/// creating or resuming a session.
pub trait SessionPickerPort: Send + Sync {
    fn choose(
        &self,
        sessions: &[SessionSummary],
        current_work_dir: &Path,
    ) -> Result<Option<String>, String>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessSessionPicker;

impl SessionPickerPort for ProcessSessionPicker {
    fn choose(
        &self,
        sessions: &[SessionSummary],
        current_work_dir: &Path,
    ) -> Result<Option<String>, String> {
        if sessions.is_empty() {
            return Err("No sessions found to resume.".to_owned());
        }
        let mut stderr = io::stderr().lock();
        writeln!(stderr, "Choose a session:").map_err(|error| error.to_string())?;
        for (index, session) in sessions.iter().enumerate() {
            let title = session
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .unwrap_or(&session.id);
            let scope = if Path::new(&session.work_dir) == current_work_dir {
                "current cwd"
            } else {
                &session.work_dir
            };
            writeln!(
                stderr,
                "  {}. {} ({}) [{}]",
                index + 1,
                title,
                session.id,
                scope
            )
            .map_err(|error| error.to_string())?;
        }

        loop {
            write!(stderr, "Session number or id (blank to cancel): ")
                .and_then(|()| stderr.flush())
                .map_err(|error| error.to_string())?;
            let mut answer = String::new();
            if io::stdin()
                .read_line(&mut answer)
                .map_err(|error| error.to_string())?
                == 0
            {
                return Ok(None);
            }
            let answer = answer.trim();
            if answer.is_empty() {
                return Ok(None);
            }
            let selected = answer
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1))
                .and_then(|index| sessions.get(index))
                .or_else(|| sessions.iter().find(|session| session.id == answer));
            let Some(selected) = selected else {
                writeln!(stderr, "No session matches {answer:?}.")
                    .map_err(|error| error.to_string())?;
                continue;
            };
            if Path::new(&selected.work_dir) != current_work_dir {
                writeln!(
                    stderr,
                    "Current session is in a different working directory.\n  To resume, run: {}",
                    resume_command(&selected.work_dir, &selected.id)
                )
                .map_err(|error| error.to_string())?;
                continue;
            }
            return Ok(Some(selected.id.clone()));
        }
    }
}

pub(crate) fn resume_command(work_dir: &str, id: &str) -> String {
    format!(
        "cd {} && mycel --resume {}",
        quote_posix(work_dir),
        quote_posix(id)
    )
}

fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_cwd_hint_quotes_shell_arguments() {
        assert_eq!(
            resume_command("/tmp/joe's repo", "session one"),
            "cd '/tmp/joe'\\''s repo' && mycel --resume 'session one'"
        );
    }
}
