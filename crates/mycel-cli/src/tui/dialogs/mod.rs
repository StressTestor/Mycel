//! Pure view-model reducers for interactive dialogs. Callers own terminal I/O
//! and execute emitted actions; reducers only transform injected input.

mod approval;
mod choice;
mod management;
mod permission;
mod question;
mod session_picker;
mod viewers;

pub use approval::*;
pub use choice::*;
pub use management::*;
pub use permission::*;
pub use question::*;
pub use session_picker::*;
pub use viewers::*;

use crate::terminal::{InputEvent, KeyCode, KeyEvent, KeyKind};

fn pressed_key(event: &InputEvent) -> Option<&KeyEvent> {
    match event {
        InputEvent::Key(key) if key.kind != KeyKind::Release => Some(key),
        _ => None,
    }
}

fn is_key(event: &InputEvent, code: KeyCode) -> bool {
    pressed_key(event).is_some_and(|key| {
        key.code == code
            && !key.modifiers.shift
            && !key.modifiers.alt
            && !key.modifiers.control
            && !key.modifiers.super_key
    })
}

fn control_char(event: &InputEvent, expected: char) -> bool {
    pressed_key(event).is_some_and(|key| {
        key.code == KeyCode::Char(expected) && key.modifiers.control && !key.modifiers.alt
    })
}

fn alt_char(event: &InputEvent, expected: char) -> bool {
    pressed_key(event).is_some_and(|key| {
        key.code == KeyCode::Char(expected) && key.modifiers.alt && !key.modifiers.control
    })
}

fn printable(event: &InputEvent) -> Option<String> {
    match event {
        InputEvent::Text(text) | InputEvent::Paste(text) => Some(text.clone()),
        InputEvent::Key(key)
            if key.kind != KeyKind::Release && !key.modifiers.control && !key.modifiers.alt =>
        {
            match key.code {
                KeyCode::Char(character) => Some(character.to_string()),
                KeyCode::Tab => Some("\t".to_owned()),
                _ => None,
            }
        }
        _ => None,
    }
}

fn printable_char(event: &InputEvent) -> Option<char> {
    let text = printable(event)?;
    let mut characters = text.chars();
    let character = characters.next()?;
    characters.next().is_none().then_some(character)
}

fn fuzzy_score(query: &str, text: &str) -> Option<i64> {
    let tokens: Vec<&str> = query
        .trim()
        .split(|character: char| character.is_whitespace() || character == '/')
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return Some(0);
    }
    tokens.into_iter().try_fold(0i64, |total, token| {
        fuzzy_token_score(token, text).map(|score| total + score)
    })
}

fn fuzzy_token_score(query: &str, text: &str) -> Option<i64> {
    let query = query.to_lowercase();
    let text = text.to_lowercase();
    match fuzzy_token_score_normalized(&query, &text) {
        Some(score) => Some(score),
        None => swapped_alpha_numeric(&query)
            .and_then(|swapped| fuzzy_token_score_normalized(&swapped, &text))
            .map(|score| score + 50),
    }
}

fn fuzzy_token_score_normalized(query: &str, text: &str) -> Option<i64> {
    let query: Vec<char> = query.chars().collect();
    let text: Vec<char> = text.chars().collect();
    if query.is_empty() {
        return Some(0);
    }
    if query.len() > text.len() {
        return None;
    }
    let mut query_index = 0usize;
    let mut score = 0i64;
    let mut last_match = None;
    let mut consecutive = 0i64;
    for (index, character) in text.iter().enumerate() {
        if query.get(query_index) != Some(character) {
            continue;
        }
        let boundary = index == 0
            || matches!(
                text[index - 1],
                ' ' | '\t' | '\n' | '-' | '_' | '.' | '/' | ':'
            );
        if last_match == index.checked_sub(1) {
            consecutive += 1;
            score -= consecutive * 50;
        } else {
            consecutive = 0;
            if let Some(previous) = last_match {
                score += i64::try_from(index - previous - 1).unwrap_or(i64::MAX) * 20;
            }
        }
        if boundary {
            score -= 100;
        }
        score += i64::try_from(index).unwrap_or(i64::MAX);
        last_match = Some(index);
        query_index += 1;
        if query_index == query.len() {
            break;
        }
    }
    if query_index != query.len() {
        return None;
    }
    if query == text {
        score -= 1_000;
    }
    Some(score)
}

fn swapped_alpha_numeric(query: &str) -> Option<String> {
    if let Some(index) = query
        .char_indices()
        .find(|(_, character)| character.is_ascii_digit())
        .map(|(index, _)| index)
    {
        let (letters, digits) = query.split_at(index);
        if !letters.is_empty()
            && letters
                .chars()
                .all(|character| character.is_ascii_lowercase())
            && !digits.is_empty()
            && digits.chars().all(|character| character.is_ascii_digit())
        {
            return Some(format!("{digits}{letters}"));
        }
    }
    let index = query
        .char_indices()
        .find(|(_, character)| character.is_ascii_lowercase())
        .map(|(index, _)| index)?;
    let (digits, letters) = query.split_at(index);
    (!digits.is_empty()
        && digits.chars().all(|character| character.is_ascii_digit())
        && !letters.is_empty()
        && letters
            .chars()
            .all(|character| character.is_ascii_lowercase()))
    .then(|| format!("{letters}{digits}"))
}
