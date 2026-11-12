use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};

use crate::media::MemoryImage;
use crate::telegram::AttachmentKind;

const MAX_CLIPBOARD: u64 = 64 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

pub enum Content {
    Text(String),
    Files(PendingAttachments),
}

pub struct PendingAttachments {
    pub paths: Vec<PathBuf>,
    temporary: Vec<PathBuf>,
    pub kind: AttachmentKind,
    pub preview: Option<MemoryImage>,
}

impl PendingAttachments {
    pub fn from_paths(paths: Vec<PathBuf>) -> Self {
        let kind = if paths.iter().all(|path| is_photo_video(path)) {
            AttachmentKind::Media
        } else {
            AttachmentKind::File
        };
        Self {
            paths,
            temporary: Vec::new(),
            kind,
            preview: None,
        }
    }

    pub fn append(&mut self, mut other: Self) {
        self.paths.append(&mut other.paths);
        self.temporary.append(&mut other.temporary);
        if self.kind != other.kind {
            self.kind = AttachmentKind::File;
        }
        if self.preview.is_none() {
            self.preview = other.preview.take();
        }
    }

    pub fn can_compress(&self) -> bool {
        self.paths.iter().all(|path| is_photo_video(path))
    }

    pub fn prepare_preview(&mut self) {
        if self.preview.is_some() {
            return;
        }
        let Some(path) = self.paths.first().filter(|path| is_photo_video(path)) else {
            return;
        };
        self.preview = command_data(
            "ffmpeg",
            &[
                "-v",
                "error",
                "-i",
                &path.to_string_lossy(),
                "-frames:v",
                "1",
                "-vf",
                "scale=320:-2",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "pipe:1",
            ],
        )
        .and_then(|bytes| MemoryImage::from_png(bytes).ok());
    }
}

impl Drop for PendingAttachments {
    fn drop(&mut self) {
        for path in &self.temporary {
            let _ = fs::remove_file(path);
        }
    }
}

fn is_photo_video(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some(
            "jpg"
                | "jpeg"
                | "png"
                | "webp"
                | "bmp"
                | "tif"
                | "tiff"
                | "heic"
                | "heif"
                | "avif"
                | "mp4"
                | "mov"
                | "mkv"
                | "webm"
        )
    )
}

fn percent_decode(value: &str) -> Option<String> {
    let mut decoded = Vec::new();
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    let value = String::from_utf8(decoded).ok()?;
    (!value.contains('\0')).then_some(value)
}

fn local_path(value: &str) -> Option<PathBuf> {
    let value = if let Some(uri) = value.strip_prefix("file://") {
        let path = if uri.starts_with('/') {
            uri
        } else {
            uri.strip_prefix("localhost")
                .filter(|path| path.starts_with('/'))?
        };
        percent_decode(path)?
    } else {
        value.to_owned()
    };
    if value.contains('\0') {
        return None;
    }
    Some(if let Some(rest) = value.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        PathBuf::from(value)
    })
}

/// Parse URI lists and shell-quoted paths without invoking a shell or expanding code.
fn path_candidates(text: &str) -> Option<Vec<PathBuf>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(path) = local_path(text)
        && path.is_file()
    {
        return Some(vec![path]);
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    for c in text.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some(c) {
            quote = None;
            continue;
        }
        if quote.is_none() && matches!(c, '\'' | '"') {
            quote = Some(c);
            continue;
        }
        if quote.is_none() && c.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(c);
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !word.is_empty() {
        words.push(word);
    }
    let paths: Option<Vec<_>> = words
        .iter()
        .filter(|word| !word.starts_with('#') && !matches!(word.as_str(), "copy" | "cut"))
        .map(|word| local_path(word))
        .collect();
    paths.filter(|paths| !paths.is_empty())
}

pub fn pasted_paths(text: &str) -> Option<PendingAttachments> {
    let paths = path_candidates(text)?;
    paths
        .iter()
        .all(|path| path.is_file())
        .then(|| PendingAttachments::from_paths(paths))
}

fn command_data(program: &str, arguments: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        let result = stdout.take(MAX_CLIPBOARD + 1).read_to_end(&mut data);
        let _ = tx.send(result.ok().map(|_| data));
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut data = None;
    let status = loop {
        if let Ok(result) = rx.try_recv() {
            data = result;
        }
        if std::time::Instant::now() > deadline
            || data
                .as_ref()
                .is_some_and(|d| d.len() as u64 > MAX_CLIPBOARD)
        {
            let _ = child.kill();
            break child.wait().ok();
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(_) => break None,
            _ => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let _ = reader.join();
    if data.is_none() {
        data = rx.try_recv().ok().flatten();
    }
    let data = data?;
    (status?.success() && !data.is_empty() && data.len() as u64 <= MAX_CLIPBOARD).then_some(data)
}

fn store_image(cache: &Path, bytes: &[u8], extension: &str) -> Result<PendingAttachments> {
    fs::create_dir_all(cache)?;
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = cache.join(format!("clipboard-{}-{id}.{extension}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    use std::io::Write;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    let mut pending = PendingAttachments::from_paths(vec![path.clone()]);
    pending.temporary.push(path);
    pending.prepare_preview();
    Ok(pending)
}

pub fn read(cache: &Path) -> Result<Content> {
    #[cfg(target_os = "macos")]
    {
        // Clipboard content is data, never interpolated into AppleScript or a shell.
        let files_script = "set paths to {}\ntry\nset itemsList to the clipboard as list\nrepeat with anItem in itemsList\nset end of paths to POSIX path of (anItem as alias)\nend repeat\nset AppleScript's text item delimiters to linefeed\nreturn paths as text\nend try";
        if let Some(bytes) = command_data("/usr/bin/osascript", &["-e", files_script])
            && let Ok(text) = String::from_utf8(bytes)
            && let Some(mut files) = pasted_paths(&text)
        {
            files.prepare_preview();
            return Ok(Content::Files(files));
        }
        // Read PNG/TIFF data descriptors through a pipe, never an arbitrary
        // clipboard-supplied output filename.
        for (class, extension) in [("PNGf", "png"), ("TIFF", "tiff")] {
            let script =
                format!("set imageData to the clipboard as «class {class}»\nreturn imageData");
            if let Some(bytes) = command_data("/usr/bin/osascript", &["-e", &script]) {
                // AppleScript prints binary data as «data TYPE<hex>», not PNG.
                if let Some(bytes) = applescript_bytes(&bytes, class) {
                    let files = store_image(cache, &bytes, extension)?;
                    return Ok(Content::Files(files));
                }
            }
        }
        if let Some(bytes) = command_data("/usr/bin/pbpaste", &[]) {
            return text_content(bytes);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        for (program, args) in [
            ("wl-paste", vec!["--no-newline", "--type", "text/uri-list"]),
            (
                "xclip",
                vec![
                    "-selection",
                    "clipboard",
                    "-target",
                    "text/uri-list",
                    "-out",
                ],
            ),
        ] {
            if let Some(bytes) = command_data(program, &args)
                && let Ok(text) = String::from_utf8(bytes)
                && let Some(mut files) = pasted_paths(&text)
            {
                files.prepare_preview();
                return Ok(Content::Files(files));
            }
        }
        for (program, args) in [
            ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
            (
                "xclip",
                vec!["-selection", "clipboard", "-target", "image/png", "-out"],
            ),
        ] {
            if let Some(bytes) = command_data(program, &args)
                && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            {
                return Ok(Content::Files(store_image(cache, &bytes, "png")?));
            }
        }
        for (program, args) in [
            ("wl-paste", vec!["--no-newline"]),
            ("xclip", vec!["-selection", "clipboard", "-out"]),
        ] {
            if let Some(bytes) = command_data(program, &args) {
                return text_content(bytes);
            }
        }
    }
    bail!(
        "clipboard is empty or unavailable; paste file paths with your terminal (Linux needs wl-paste or xclip)"
    )
}

fn text_content(bytes: Vec<u8>) -> Result<Content> {
    anyhow::ensure!(
        bytes.len() <= 4 * 1024 * 1024,
        "clipboard text exceeds 4 MiB"
    );
    let text = String::from_utf8(bytes).context("clipboard text is not UTF-8")?;
    if let Some(mut files) = pasted_paths(&text) {
        files.prepare_preview();
        Ok(Content::Files(files))
    } else {
        Ok(Content::Text(text))
    }
}

#[cfg(target_os = "macos")]
fn applescript_bytes(data: &[u8], class: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(data).ok()?.trim();
    let hex = text
        .strip_prefix(&format!("«data {class}"))?
        .strip_suffix('»')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    hex.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            Some(((pair[0] as char).to_digit(16)? * 16 + (pair[1] as char).to_digit(16)?) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_uris_and_quoted_unicode_paths_without_shell_expansion() {
        let paths = path_candidates(
            "copy\nfile:///tmp/My%20photo%20%D1%8F.png\nfile://localhost/tmp/b.txt",
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                PathBuf::from("/tmp/My photo я.png"),
                PathBuf::from("/tmp/b.txt")
            ]
        );
        assert_eq!(
            path_candidates("'/tmp/hello world.png' /tmp/other\\ photo.jpg").unwrap(),
            [
                PathBuf::from("/tmp/hello world.png"),
                PathBuf::from("/tmp/other photo.jpg")
            ]
        );
        assert!(local_path("file://remote.example/tmp/private").is_none());
        assert!(local_path("file:///tmp/%00secret").is_none());
        assert_eq!(
            local_path("$(touch /tmp/never-run)"),
            Some(PathBuf::from("$(touch /tmp/never-run)"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_applescript_binary_descriptor_without_executing_it() {
        assert_eq!(
            applescript_bytes("«data PNGf89504E470D0A1A0A»\n".as_bytes(), "PNGf").unwrap(),
            b"\x89PNG\r\n\x1a\n"
        );
        assert!(applescript_bytes("«data PNGf0g»".as_bytes(), "PNGf").is_none());
    }

    #[test]
    fn canceling_only_removes_owned_clipboard_files_not_user_originals() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("telega-clipboard-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let original = dir.join("user.png");
        let temporary = dir.join("clipboard.png");
        fs::write(&original, b"original").unwrap();
        fs::write(&temporary, b"clipboard").unwrap();
        let mut pending = PendingAttachments::from_paths(vec![original.clone(), temporary.clone()]);
        pending.temporary.push(temporary.clone());
        drop(pending);
        assert_eq!(fs::read(&original).unwrap(), b"original");
        assert!(!temporary.exists());
        fs::remove_file(original).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
