use std::str;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyKind {
    #[default]
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: Modifiers,
    pub kind: KeyKind,
}

impl KeyEvent {
    pub fn press(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: Modifiers::default(),
            kind: KeyKind::Press,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    Text(String),
    Paste(String),
    Unknown(Vec<u8>),
}

/// Incremental raw-terminal decoder. Partial UTF-8, CSI, OSC, DCS, APC, and
/// bracketed-paste sequences stay buffered until a complete event arrives.
#[derive(Debug, Default)]
pub struct InputDecoder {
    buffer: Vec<u8>,
    paste: Option<Vec<u8>>,
}

impl InputDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if self.paste.is_some() {
                if !self.consume_paste(&mut events) {
                    break;
                }
                continue;
            }
            if self.buffer.is_empty() {
                break;
            }
            if self.buffer.starts_with(PASTE_START) {
                self.buffer.drain(..PASTE_START.len());
                self.paste = Some(Vec::new());
                continue;
            }

            if self.buffer[0] == 0x1b {
                match self.consume_escape() {
                    Consume::Event(event) => events.push(event),
                    Consume::Skipped => {}
                    Consume::Incomplete => break,
                }
                continue;
            }

            let byte = self.buffer[0];
            if let Some(event) = control_event(byte) {
                self.buffer.remove(0);
                events.push(InputEvent::Key(event));
                continue;
            }

            match next_utf8(&self.buffer) {
                Utf8::Complete(character, length) => {
                    self.buffer.drain(..length);
                    events.push(InputEvent::Text(character.to_string()));
                }
                Utf8::Incomplete => break,
                Utf8::Invalid => {
                    let invalid = self.buffer.remove(0);
                    events.push(InputEvent::Unknown(vec![invalid]));
                }
            }
        }
        events
    }

    pub fn flush(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();
        if let Some(mut paste) = self.paste.take() {
            paste.append(&mut self.buffer);
            events.push(InputEvent::Paste(
                String::from_utf8_lossy(&paste).into_owned(),
            ));
        } else if self.buffer == [0x1b] {
            self.buffer.clear();
            events.push(InputEvent::Key(KeyEvent::press(KeyCode::Escape)));
        } else if !self.buffer.is_empty() {
            events.push(InputEvent::Unknown(std::mem::take(&mut self.buffer)));
        }
        events
    }

    fn consume_paste(&mut self, events: &mut Vec<InputEvent>) -> bool {
        let Some(index) = find_bytes(&self.buffer, PASTE_END) else {
            let keep = suffix_prefix_overlap(&self.buffer, PASTE_END);
            let take = self.buffer.len().saturating_sub(keep);
            if take > 0 {
                let bytes: Vec<u8> = self.buffer.drain(..take).collect();
                self.paste
                    .as_mut()
                    .expect("paste mode is active")
                    .extend(bytes);
            }
            return false;
        };
        let bytes: Vec<u8> = self.buffer.drain(..index).collect();
        let mut paste = self.paste.take().expect("paste mode is active");
        paste.extend(bytes);
        self.buffer.drain(..PASTE_END.len());
        events.push(InputEvent::Paste(
            String::from_utf8_lossy(&paste).into_owned(),
        ));
        true
    }

    fn consume_escape(&mut self) -> Consume {
        if self.buffer.len() == 1 {
            return Consume::Incomplete;
        }
        match self.buffer[1] {
            b'[' => self.consume_csi(),
            b']' => self.consume_string_sequence(2, true),
            b'P' | b'_' | b'^' => self.consume_string_sequence(2, false),
            b'\r' => {
                self.buffer.drain(..2);
                Consume::Event(InputEvent::Key(KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: Modifiers {
                        shift: true,
                        ..Modifiers::default()
                    },
                    kind: KeyKind::Press,
                }))
            }
            _ => match next_utf8(&self.buffer[1..]) {
                Utf8::Complete(character, length) => {
                    self.buffer.drain(..1 + length);
                    Consume::Event(InputEvent::Key(KeyEvent {
                        code: KeyCode::Char(character),
                        modifiers: Modifiers {
                            alt: true,
                            ..Modifiers::default()
                        },
                        kind: KeyKind::Press,
                    }))
                }
                Utf8::Incomplete => Consume::Incomplete,
                Utf8::Invalid => {
                    let bytes: Vec<u8> = self.buffer.drain(..2).collect();
                    Consume::Event(InputEvent::Unknown(bytes))
                }
            },
        }
    }

    fn consume_csi(&mut self) -> Consume {
        let Some(end) = self.buffer[2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
            .map(|offset| offset + 2)
        else {
            return Consume::Incomplete;
        };
        let sequence: Vec<u8> = self.buffer.drain(..=end).collect();
        if sequence == PASTE_START {
            self.paste = Some(Vec::new());
            return Consume::Skipped;
        }
        if sequence == PASTE_END {
            return Consume::Event(InputEvent::Unknown(sequence));
        }
        match parse_csi(&sequence) {
            Some(event) => Consume::Event(InputEvent::Key(event)),
            None => Consume::Event(InputEvent::Unknown(sequence)),
        }
    }

    fn consume_string_sequence(&mut self, start: usize, allow_bel: bool) -> Consume {
        let mut index = start;
        while index < self.buffer.len() {
            if allow_bel && self.buffer[index] == 0x07 {
                self.buffer.drain(..=index);
                return Consume::Skipped;
            }
            if self.buffer[index] == 0x1b && self.buffer.get(index + 1).copied() == Some(b'\\') {
                self.buffer.drain(..index + 2);
                return Consume::Skipped;
            }
            index += 1;
        }
        Consume::Incomplete
    }
}

enum Consume {
    Event(InputEvent),
    Skipped,
    Incomplete,
}

enum Utf8 {
    Complete(char, usize),
    Incomplete,
    Invalid,
}

fn next_utf8(bytes: &[u8]) -> Utf8 {
    let Some(first) = bytes.first().copied() else {
        return Utf8::Incomplete;
    };
    let length = match first {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Utf8::Invalid,
    };
    if bytes.len() < length {
        return Utf8::Incomplete;
    }
    match str::from_utf8(&bytes[..length]) {
        Ok(value) => Utf8::Complete(
            value.chars().next().expect("UTF-8 slice is non-empty"),
            length,
        ),
        Err(_) => Utf8::Invalid,
    }
}

fn control_event(byte: u8) -> Option<KeyEvent> {
    let event = match byte {
        b'\r' => KeyEvent::press(KeyCode::Enter),
        b'\n' => modified_char(
            'j',
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        ),
        b'\t' => KeyEvent::press(KeyCode::Tab),
        0x7f | 0x08 => KeyEvent::press(KeyCode::Backspace),
        0x01..=0x1a => modified_char(
            char::from(b'a' + byte - 1),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        ),
        0x1c..=0x1f => modified_char(
            char::from(b'\\' + byte - 0x1c),
            Modifiers {
                control: true,
                ..Modifiers::default()
            },
        ),
        _ => return None,
    };
    Some(event)
}

fn parse_csi(sequence: &[u8]) -> Option<KeyEvent> {
    let final_byte = *sequence.last()?;
    let parameters = str::from_utf8(&sequence[2..sequence.len() - 1]).ok()?;
    if final_byte == b'u' {
        return parse_kitty_key(parameters);
    }
    if final_byte == b'Z' {
        return Some(KeyEvent {
            code: KeyCode::Tab,
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::default()
            },
            kind: KeyKind::Press,
        });
    }
    if final_byte == b'~' {
        let parts: Vec<&str> = parameters.split(';').collect();
        if parts.first().copied() == Some("27") && parts.len() >= 3 {
            let modifiers = decode_modifiers(parts[1].parse().ok()?);
            let code = key_code_from_codepoint(parts[2].parse().ok()?, modifiers)?;
            return Some(KeyEvent {
                code,
                modifiers,
                kind: KeyKind::Press,
            });
        }
        if parts.first().copied() == Some("13") {
            let modifiers = parts
                .get(1)
                .and_then(|value| value.parse().ok())
                .map(decode_modifiers)
                .unwrap_or_default();
            return Some(KeyEvent {
                code: KeyCode::Enter,
                modifiers,
                kind: KeyKind::Press,
            });
        }
        let code = match parts.first().copied()? {
            "1" | "7" => KeyCode::Home,
            "3" => KeyCode::Delete,
            "4" | "8" => KeyCode::End,
            "5" => KeyCode::PageUp,
            "6" => KeyCode::PageDown,
            _ => return None,
        };
        return Some(KeyEvent::press(code));
    }

    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        _ => return None,
    };
    let modifier = parameters
        .split(';')
        .next_back()
        .and_then(|part| part.parse().ok())
        .filter(|_| parameters.contains(';'))
        .unwrap_or(1);
    Some(KeyEvent {
        code,
        modifiers: decode_modifiers(modifier),
        kind: KeyKind::Press,
    })
}

fn parse_kitty_key(parameters: &str) -> Option<KeyEvent> {
    let mut fields = parameters.split(';');
    let codepoint: u32 = fields.next()?.split(':').next()?.parse().ok()?;
    let modifier_field = fields.next().unwrap_or("1");
    let mut modifier_parts = modifier_field.split(':');
    let modifiers = decode_modifiers(modifier_parts.next()?.parse().ok()?);
    let kind = match modifier_parts.next().unwrap_or("1") {
        "2" => KeyKind::Repeat,
        "3" => KeyKind::Release,
        _ => KeyKind::Press,
    };
    let code = key_code_from_codepoint(codepoint, modifiers)?;
    Some(KeyEvent {
        code,
        modifiers,
        kind,
    })
}

fn key_code_from_codepoint(value: u32, modifiers: Modifiers) -> Option<KeyCode> {
    match value {
        13 => Some(KeyCode::Enter),
        27 => Some(KeyCode::Escape),
        127 => Some(KeyCode::Backspace),
        value => {
            let character = char::from_u32(value)?;
            Some(KeyCode::Char(if modifiers.control {
                character.to_ascii_lowercase()
            } else {
                character
            }))
        }
    }
}

fn decode_modifiers(encoded: u16) -> Modifiers {
    let bits = encoded.saturating_sub(1);
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        control: bits & 4 != 0,
        super_key: bits & 8 != 0,
        caps_lock: bits & 64 != 0,
        num_lock: bits & 128 != 0,
    }
}

fn modified_char(character: char, modifiers: Modifiers) -> KeyEvent {
    KeyEvent {
        code: KeyCode::Char(character),
        modifiers,
        kind: KeyKind::Press,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn suffix_prefix_overlap(bytes: &[u8], prefix: &[u8]) -> usize {
    (1..=bytes.len().min(prefix.len()))
        .rev()
        .find(|length| bytes[bytes.len() - length..] == prefix[..*length])
        .unwrap_or(0)
}
