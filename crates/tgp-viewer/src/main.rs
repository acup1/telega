use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use terminalib::{
    WindowManager,
    terminal::Terminal,
    tgp::{ImageData, query_support},
};
use tgp_viewer::{PlaybackOptions, VideoInputSource, ViewerAction};

const HELP: &str = "tgp-viewer [--] FILE [FILE ...]\n\n\
View local images and videos in a terminal. Left/Right changes files.\n\
Esc or a click outside closes the viewer. Space pauses or resumes video.\n\
Click or drag the timeline to seek. Video uses ffmpeg, ffprobe and rsvg-convert.\n\
Sound uses mpv, ffplay or afplay. A terminal with Kitty graphics support displays frames.";

struct LocalVideo<'a> {
    path: &'a Path,
    size: u64,
}

impl VideoInputSource for LocalVideo<'_> {
    fn input(&self) -> &OsStr {
        self.path.as_os_str()
    }
    fn buffered(&self) -> (u64, u64) {
        (self.size, self.size)
    }
}

enum MediaKind {
    Png,
    Image,
    Video(f64),
}

fn probe(path: &Path) -> Result<MediaKind> {
    let mut signature = [0; 8];
    if File::open(path)?.read_exact(&mut signature).is_ok() && &signature == b"\x89PNG\r\n\x1a\n" {
        return Ok(MediaKind::Png);
    }
    let info = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "V:0",
            "-show_entries",
            "stream=codec_type:format=format_name,duration",
            "-of",
            "default=noprint_wrappers=1",
            "-i",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("ffprobe is required to identify this media")?;
    let info_text = String::from_utf8_lossy(&info.stdout);
    ensure!(
        info.status.success() && info_text.lines().any(|line| line == "codec_type=video"),
        "no supported image or video stream"
    );
    let field = |name: &str| {
        info_text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (name == key).then_some(value)
        })
    };
    let format = field("format_name").unwrap_or_default();
    if format.contains("_pipe")
        || format == "image2"
        || path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("avif"))
    {
        return Ok(MediaKind::Image);
    }
    let duration = field("duration")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or_default();
    Ok(MediaKind::Video(duration))
}

fn decode_image(path: &Path) -> Result<ImageData> {
    let png = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:V:0",
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .output()
        .context("ffmpeg is required to convert this image")?;
    ensure!(png.status.success(), "could not decode image");
    Ok(ImageData::from_png_bytes(png.stdout)?)
}

async fn gallery(files: &[PathBuf], terminal: &mut Terminal, supported: bool) -> Result<()> {
    let mut windows = WindowManager::new();
    let nerd_fonts = std::env::var("TELEGA_NERD_FONTS").as_deref() != Ok("0");
    let mut position = 0;
    loop {
        let path = &files[position];
        let action = if !supported {
            tgp_viewer::show_unsupported(terminal, &mut windows, nerd_fonts)
        } else {
            match probe(path).with_context(|| format!("cannot open {}", path.display()))? {
                MediaKind::Png => tgp_viewer::show_tgp(path, terminal, &mut windows, false),
                MediaKind::Image => {
                    tgp_viewer::show_image(decode_image(path)?, terminal, &mut windows, false)
                }
                MediaKind::Video(duration) => {
                    let source = LocalVideo {
                        path,
                        size: path.metadata()?.len(),
                    };
                    tgp_viewer::play_video(
                        &source,
                        duration,
                        None,
                        terminal,
                        &mut windows,
                        PlaybackOptions::default(),
                    )
                    .await
                }
            }
        }
        .with_context(|| format!("cannot display {}", path.display()))?;
        match action {
            ViewerAction::Previous => position = position.saturating_sub(1),
            ViewerAction::Next => position = (position + 1).min(files.len() - 1),
            ViewerAction::Close => return Ok(()),
            ViewerAction::React | ViewerAction::Reply => unreachable!("story mode is disabled"),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut files = Vec::new();
    let mut literal = false;
    for arg in std::env::args_os().skip(1) {
        if !literal {
            if arg == "--" {
                literal = true;
                continue;
            }
            if arg == "--help" || arg == "-h" {
                println!("{HELP}");
                return Ok(());
            }
            if arg.as_encoded_bytes().starts_with(b"-") {
                bail!("unknown option {arg:?}. Use -- before filenames beginning with a dash");
            }
        }
        let path = fs::canonicalize(&arg).with_context(|| format!("cannot open {arg:?}"))?;
        ensure!(path.is_file(), "expected a file: {}", path.display());
        files.push(path);
    }
    ensure!(!files.is_empty(), "{HELP}");
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "tgp-viewer needs an interactive terminal"
    );
    let mut terminal = Terminal::wannajustwork()?;
    let supported = query_support(&mut terminal, Duration::from_millis(500))?;
    let result = gallery(&files, &mut terminal, supported).await;
    let cleanup = if supported {
        tgp_viewer::clear_viewer_layers(&mut terminal)
    } else {
        Ok(())
    };
    let restore = terminal.restore();
    result?;
    cleanup?;
    Ok(restore?)
}
