use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use mycel_agent_protocol::{ContentPart, MediaUrl};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_CLIPBOARD_STDOUT: usize = MAX_IMAGE_BYTES * 4 / 3 + 4096;
const MAX_ATTACHMENTS: usize = 16;
const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

#[derive(Clone, Debug)]
struct PastedImage {
    placeholder: String,
    part: ContentPart,
    bytes: usize,
}

#[derive(Debug, Default)]
pub struct PastedImageStore {
    next_id: u64,
    total_bytes: usize,
    images: BTreeMap<u64, PastedImage>,
}

impl PastedImageStore {
    pub fn add(&mut self, image: ClipboardImage) -> Result<String, String> {
        if image.bytes.is_empty() || image.bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "clipboard image must be between 1 byte and {MAX_IMAGE_BYTES} bytes"
            ));
        }
        if sniff_image(&image.bytes) != Some(image.mime) {
            return Err("clipboard image bytes do not match the reported format".to_owned());
        }
        while self.images.len() >= MAX_ATTACHMENTS
            || self.total_bytes.saturating_add(image.bytes.len()) > MAX_ATTACHMENT_BYTES
        {
            let Some(oldest) = self.images.keys().next().copied() else {
                return Err("clipboard image store is full".to_owned());
            };
            if let Some(removed) = self.images.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            }
        }
        self.next_id = self.next_id.saturating_add(1).max(1);
        let id = self.next_id;
        let placeholder = format!("[image #{id}]");
        let part = ContentPart::ImageUrl {
            image_url: MediaUrl {
                url: format!(
                    "data:{};base64,{}",
                    image.mime,
                    BASE64_STANDARD.encode(&image.bytes)
                ),
                id: Some(format!("clipboard-{id}")),
            },
        };
        self.total_bytes = self.total_bytes.saturating_add(image.bytes.len());
        self.images.insert(
            id,
            PastedImage {
                placeholder: placeholder.clone(),
                part,
                bytes: image.bytes.len(),
            },
        );
        Ok(placeholder)
    }

    pub fn expand(&self, text: &str) -> Vec<ContentPart> {
        let mut parts = Vec::new();
        let mut search_cursor = 0usize;
        let mut emitted_cursor = 0usize;
        while let Some(relative) = text[search_cursor..].find("[image #") {
            let start = search_cursor + relative;
            let Some(close_relative) = text[start..].find(']') else {
                break;
            };
            let end = start + close_relative + 1;
            let literal = &text[start..end];
            let id = literal
                .strip_prefix("[image #")
                .and_then(|value| value.strip_suffix(']'))
                .and_then(|value| value.parse::<u64>().ok());
            let Some(image) = id.and_then(|id| self.images.get(&id)) else {
                search_cursor = end;
                continue;
            };
            if image.placeholder != literal {
                search_cursor = end;
                continue;
            }
            push_text(&mut parts, &text[emitted_cursor..start]);
            parts.push(image.part.clone());
            emitted_cursor = end;
            search_cursor = end;
        }
        if parts.is_empty() {
            return vec![ContentPart::text(text)];
        }
        push_text(&mut parts, &text[emitted_cursor..]);
        parts
    }
}

fn push_text(parts: &mut Vec<ContentPart>, text: &str) {
    if !text.is_empty() {
        parts.push(ContentPart::text(text));
    }
}

pub fn read_clipboard_image() -> Result<Option<ClipboardImage>, String> {
    #[cfg(target_os = "macos")]
    {
        if let Some(image) = read_macos_file_image()? {
            return Ok(Some(image));
        }
        read_macos_png()
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(image) = read_linux_file_image()? {
            return Ok(Some(image));
        }
        return read_linux_image();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    Ok(None)
}

#[cfg(target_os = "macos")]
fn read_macos_file_image() -> Result<Option<ClipboardImage>, String> {
    const SCRIPT: &str = r#"
ObjC.import('AppKit');
ObjC.import('Foundation');
const out = [];
const pb = $.NSPasteboard.generalPasteboard;
try {
  const options = $.NSMutableDictionary.dictionary;
  options.setObjectForKey($.NSNumber.numberWithBool(true), $.NSPasteboardURLReadingFileURLsOnlyKey);
  const urls = pb.readObjectsForClassesOptions([$.NSURL], options);
  const count = urls ? urls.count : 0;
  for (let i = 0; i < count; i++) {
    const value = urls.objectAtIndex(i).path;
    const path = value ? ObjC.unwrap(value) : '';
    if (path) out.push(path);
  }
} catch (error) {}
out.join('\n');
"#;
    let output = run_bounded("osascript", &["-l", "JavaScript", "-e", SCRIPT], None)?;
    if !output.success {
        return Ok(None);
    }
    if output.oversized {
        return Err("clipboard file list exceeds the bounded output limit".to_owned());
    }
    for path in parse_clipboard_paths(&output.stdout) {
        if let Some(image) = read_path_image(&path)? {
            return Ok(Some(image));
        }
    }
    Ok(None)
}

#[cfg(target_os = "macos")]
fn read_macos_png() -> Result<Option<ClipboardImage>, String> {
    use std::os::unix::fs::DirBuilderExt;

    const SCRIPT: &str = r#"
ObjC.import('AppKit');
ObjC.import('Foundation');
const env = $.NSProcessInfo.processInfo.environment;
const rawPath = env.objectForKey('MYCEL_CLIPBOARD_OUTPUT');
const path = rawPath ? ObjC.unwrap(rawPath) : '';
const data = $.NSPasteboard.generalPasteboard.dataForType('public.png');
if (path && data && data.length > 0) data.writeToFileAtomically(path, true);
"#;
    let directory =
        std::env::temp_dir().join(format!("mycel-clipboard-{}", uuid::Uuid::new_v4().simple()));
    let path = directory.join("clipboard.png");
    let result = (|| {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&directory).map_err(|error| {
            format!(
                "could not create clipboard staging directory {}: {error}",
                directory.display()
            )
        })?;
        let output = run_bounded(
            "osascript",
            &["-l", "JavaScript", "-e", SCRIPT],
            Some(("MYCEL_CLIPBOARD_OUTPUT", path.as_os_str())),
        )?;
        if !output.success {
            return Ok(None);
        }
        read_path_image(&path)
    })();
    let cleanup = fs::remove_dir_all(&directory);
    match (result, cleanup) {
        (Ok(image), Ok(())) => Ok(image),
        (Ok(None), Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        (Ok(_), Err(error)) => Err(format!("could not remove clipboard staging data: {error}")),
        (Err(error), _) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn read_linux_file_image() -> Result<Option<ClipboardImage>, String> {
    for (program, arguments) in [
        ("wl-paste", vec!["--type", "text/uri-list", "--no-newline"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "text/uri-list", "-o"],
        ),
    ] {
        let output = run_bounded(program, &arguments, None)?;
        if !output.success {
            continue;
        }
        for path in parse_clipboard_paths(&output.stdout) {
            if let Some(image) = read_path_image(&path)? {
                return Ok(Some(image));
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "linux")]
fn read_linux_image() -> Result<Option<ClipboardImage>, String> {
    for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
        for (program, arguments) in [
            ("wl-paste", vec!["--type", mime, "--no-newline"]),
            ("xclip", vec!["-selection", "clipboard", "-t", mime, "-o"]),
        ] {
            let output = run_bounded(program, &arguments, None)?;
            if output.success && !output.stdout.is_empty() {
                if output.oversized {
                    return Err("clipboard image exceeds the 20 MiB limit".to_owned());
                }
                if let Some(detected) = sniff_image(&output.stdout) {
                    return Ok(Some(ClipboardImage {
                        bytes: output.stdout,
                        mime: detected,
                    }));
                }
            }
        }
    }
    Ok(None)
}

fn parse_clipboard_paths(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .split(['\r', '\n', '\0'])
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            if line.starts_with("file://") {
                return url::Url::parse(line).ok()?.to_file_path().ok();
            }
            let path = Path::new(line);
            path.is_absolute().then(|| path.to_path_buf())
        })
        .collect()
}

fn read_path_image(path: &Path) -> Result<Option<ClipboardImage>, String> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect clipboard image: {error}"))?;
    if !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() == 0 || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return if metadata.len() > MAX_IMAGE_BYTES as u64 {
            Err("clipboard image exceeds the 20 MiB limit".to_owned())
        } else {
            Ok(None)
        };
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("could not read clipboard image: {error}"))?;
    Ok(sniff_image(&bytes).map(|mime| ClipboardImage { bytes, mime }))
}

fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

struct BoundedOutput {
    success: bool,
    stdout: Vec<u8>,
    oversized: bool,
}

fn run_bounded(
    program: &str,
    arguments: &[&str],
    environment: Option<(&str, &std::ffi::OsStr)>,
) -> Result<BoundedOutput, String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some((name, value)) = environment {
        command.env(name, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(BoundedOutput {
                success: false,
                stdout: Vec::new(),
                oversized: false,
            });
        }
        Err(error) => {
            return Err(format!(
                "could not start clipboard helper {program}: {error}"
            ))
        }
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("clipboard helper {program} has no stdout pipe"))?;
    let reader = thread::spawn(move || {
        let mut kept = Vec::new();
        let mut oversized = false;
        let mut buffer = [0u8; 8192];
        loop {
            let count = stdout.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let remaining = MAX_CLIPBOARD_STDOUT.saturating_sub(kept.len());
            kept.extend_from_slice(&buffer[..count.min(remaining)]);
            oversized |= count > remaining;
        }
        Ok::<_, io::Error>((kept, oversized))
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("clipboard helper {program} timed out"));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(format!("clipboard helper {program} failed: {error}"));
            }
        }
    };
    let (stdout, oversized) = reader
        .join()
        .map_err(|_| format!("clipboard helper {program} output reader panicked"))?
        .map_err(|error| format!("could not read clipboard helper {program}: {error}"))?;
    Ok(BoundedOutput {
        success: status.success(),
        stdout,
        oversized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_store_expands_only_owned_placeholders_in_order() {
        let mut store = PastedImageStore::default();
        let first = store
            .add(ClipboardImage {
                bytes: b"\x89PNG\r\n\x1a\nfirst".to_vec(),
                mime: "image/png",
            })
            .expect("first image");
        let second = store
            .add(ClipboardImage {
                bytes: b"GIF89asecond".to_vec(),
                mime: "image/gif",
            })
            .expect("second image");
        let parts = store.expand(&format!(
            "before {first} middle [image #999] {second} after"
        ));
        assert_eq!(parts.len(), 5);
        assert!(matches!(parts[1], ContentPart::ImageUrl { .. }));
        assert!(matches!(parts[3], ContentPart::ImageUrl { .. }));
        assert_eq!(parts[2].as_text(), Some(" middle [image #999] "));
    }

    #[test]
    fn image_sniffing_and_file_url_parsing_are_strict() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\nbody"), Some("image/png"));
        assert_eq!(sniff_image(b"not an image"), None);
        let paths = parse_clipboard_paths(b"# comment\nfile:///tmp/a%20b.png\nrelative.png\n");
        assert_eq!(paths, vec![PathBuf::from("/tmp/a b.png")]);
    }
}
