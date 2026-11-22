use crate::display::*;
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, Instant};
use terminalib::tgp::{ImageData, ImageGroup, TerminalImage};
use terminalib::{KEY_LEFT, KEY_RIGHT, MOUSE_EVENT, MouseEvent, WindowManager, terminal::Terminal};

pub trait VideoInputSource {
    /// Absolute path or seekable stream URL accepted by the decoder.
    fn input(&self) -> &OsStr;
    /// Loaded and total bytes, displayed by the timeline.
    fn buffered(&self) -> (u64, u64);
}

pub struct PlaybackOptions {
    pub advance_on_end: bool,
    pub story_mode: bool,
    pub stderr: fn() -> Stdio,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            advance_on_end: false,
            story_mode: false,
            stderr: Stdio::null,
        }
    }
}

struct VideoPipeline {
    decoder: Child,
    audio: Option<Child>,
    frames: mpsc::Receiver<std::result::Result<Vec<u8>, String>>,
    reader: Option<thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    audio_started: bool,
}

impl VideoPipeline {
    fn set_paused(&self, paused: bool) {
        set_child_paused(&self.decoder, paused);
        if let Some(audio) = &self.audio {
            set_child_paused(audio, paused);
        }
    }

    fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.set_paused(false);
        let _ = self.decoder.kill();
        let _ = self.decoder.wait();
        if let Some(audio) = &mut self.audio {
            let _ = audio.kill();
            let _ = audio.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bounded, non-blocking player for GIFs and round video messages in the feed.
/// Only two decoded frames and the range bridge's bounded byte cache are held.
pub struct InlinePlayback<S> {
    pipeline: VideoPipeline,
    source: S,
    pub frame: Option<MemoryImage>,
    pub serial: usize,
    pub position: f64,
    pub duration: f64,
    pub paused: bool,
    pub ended: bool,
    offset: f64,
    frames: u64,
    options: DecoderOptions,
}

impl<S: VideoInputSource> InlinePlayback<S> {
    pub fn new(
        source: S,
        duration: f64,
        looping: bool,
        audible: bool,
        round: bool,
        stderr: fn() -> Stdio,
    ) -> Result<Self> {
        let options = DecoderOptions {
            round,
            stderr,
            looping,
            compatible: false,
        };
        let mut pipeline = spawn_video_pipeline_with(source.input(), 0.0, 10, 256, options)?;
        // The audio process is started only when the first frame arrives.
        pipeline.audio_started = !audible;
        Ok(Self {
            pipeline,
            source,
            frame: None,
            serial: 0,
            position: 0.0,
            duration,
            paused: false,
            ended: false,
            offset: 0.0,
            frames: 0,
            options,
        })
    }

    pub fn tick(&mut self) -> Result<bool> {
        if self.paused || self.ended {
            return Ok(false);
        }
        match self.pipeline.frames.try_recv() {
            Ok(Ok(png)) => {
                self.frame = Some(MemoryImage::from_png(png)?);
                self.frames += 1;
                self.position = self.offset + self.frames as f64 / 10.0;
                self.serial = self.serial.wrapping_add(1);
                if !self.pipeline.audio_started {
                    self.pipeline.audio =
                        Some(spawn_audio_input(self.source.input(), self.offset)?);
                    self.pipeline.audio_started = true;
                }
                Ok(true)
            }
            result @ (Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected)) => {
                if self.frames == 0 && !self.options.compatible {
                    self.options.compatible = true;
                    let audible = !self.pipeline.audio_started || self.pipeline.audio.is_some();
                    self.pipeline.stop();
                    self.pipeline = spawn_video_pipeline_with(
                        self.source.input(),
                        self.offset,
                        10,
                        256,
                        self.options,
                    )?;
                    self.pipeline.audio_started = !audible;
                    return Ok(false);
                }
                self.pipeline.stop();
                self.ended = true;
                self.paused = true;
                if self.frames == 0 {
                    anyhow::bail!(
                        "{}",
                        match result {
                            Ok(Err(error)) => error,
                            _ => "inline decoder produced no frames".to_owned(),
                        }
                    );
                }
                Ok(true)
            }
            Err(mpsc::TryRecvError::Empty) => Ok(false),
        }
    }

    pub fn toggle(&mut self) -> Result<()> {
        if self.ended {
            return self.seek(0.0);
        }
        self.paused = !self.paused;
        self.pipeline.set_paused(self.paused);
        Ok(())
    }

    pub fn seek(&mut self, ratio: f64) -> Result<()> {
        let audible = !self.pipeline.audio_started || self.pipeline.audio.is_some();
        self.pipeline.stop();
        self.offset = (self.duration * ratio.clamp(0.0, 1.0)).min((self.duration - 0.1).max(0.0));
        self.pipeline =
            spawn_video_pipeline_with(self.source.input(), self.offset, 10, 256, self.options)?;
        self.pipeline.audio_started = !audible;
        self.position = self.offset;
        self.frames = 0;
        self.ended = false;
        self.paused = false;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct DecoderOptions {
    stderr: fn() -> Stdio,
    compatible: bool,
    round: bool,
    looping: bool,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            compatible: false,
            round: false,
            looping: false,
            stderr: Stdio::null,
        }
    }
}

fn spawn_video_pipeline_with(
    input: &OsStr,
    offset: f64,
    fps: u32,
    decode_width: u32,
    options: DecoderOptions,
) -> Result<VideoPipeline> {
    let mut command = Command::new("ffmpeg");
    command.args(["-loglevel", "warning", "-nostdin"]);
    if input.as_encoded_bytes().starts_with(b"http://") {
        command.args([
            "-rw_timeout",
            "30000000",
            "-reconnect",
            "1",
            "-reconnect_streamed",
            "1",
            "-reconnect_delay_max",
            "2",
        ]);
    }
    if options.compatible {
        command.args([
            "-fflags",
            "+genpts+discardcorrupt",
            "-err_detect",
            "ignore_err",
            "-probesize",
            "10000000",
            "-analyzeduration",
            "10000000",
        ]);
    } else if offset > 0.0 {
        command.arg("-ss").arg(format!("{offset:.3}"));
    }
    if options.looping {
        command.args(["-stream_loop", "-1"]);
    }
    command.arg("-i").arg(input);
    if options.compatible && offset > 0.0 {
        command.arg("-ss").arg(format!("{offset:.3}"));
    }
    // Upper-case V excludes attached cover images, which otherwise decode as
    // a one-frame "video" in files with several streams.
    let filter = if options.round {
        format!(
            "fps={fps},crop='min(iw,ih)':'min(iw,ih)',scale={decode_width}:{decode_width},setsar=1,format=rgba,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='if(lte(pow(X-W/2,2)+pow(Y-H/2,2),pow(min(W,H)/2,2)),255,0)'"
        )
    } else {
        format!("fps={fps},scale={decode_width}:-2,setsar=1")
    };
    let mut decoder = command
        .args([
            "-map",
            "0:V:0",
            "-an",
            "-sn",
            "-dn",
            "-threads",
            "2",
            "-vf",
            &filter,
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-compression_level",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr((options.stderr)())
        .spawn()
        .context("ffmpeg is required for TGP video decoding")?;
    let stdout = decoder
        .stdout
        .take()
        .context("ffmpeg stdout is unavailable")?;
    let (frames_tx, frames) = mpsc::sync_channel(2);
    let stopped = Arc::new(AtomicBool::new(false));
    let reader_stopped = stopped.clone();
    let reader = thread::spawn(move || {
        let mut stream = BufReader::with_capacity(1024 * 1024, stdout);
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps.max(1)));
        let mut next_frame = Instant::now();
        loop {
            match read_png_frame(&mut stream) {
                Ok(Some(frame)) => {
                    // Pace output frames, not input reads. With ffmpeg's -re,
                    // compatible output seeking would spend the whole skipped
                    // duration reading input in real time before the first frame.
                    while let Some(remaining) = next_frame.checked_duration_since(Instant::now()) {
                        if reader_stopped.load(Ordering::Acquire) {
                            return;
                        }
                        thread::sleep(remaining.min(Duration::from_millis(5)));
                    }
                    let mut packet = Ok(frame);
                    loop {
                        if reader_stopped.load(Ordering::Acquire) {
                            return;
                        }
                        match frames_tx.try_send(packet) {
                            Ok(()) => break,
                            Err(mpsc::TrySendError::Full(value)) => {
                                packet = value;
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                        }
                    }
                    next_frame = Instant::now() + interval;
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = frames_tx.try_send(Err(error.to_string()));
                    break;
                }
            }
        }
    });
    Ok(VideoPipeline {
        decoder,
        audio: None,
        frames,
        reader: Some(reader),
        stopped,
        audio_started: false,
    })
}

pub async fn play_video(
    source: &impl VideoInputSource,
    duration: f64,
    preview: Option<MemoryImage>,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    options: PlaybackOptions,
) -> Result<ViewerAction> {
    let PlaybackOptions {
        advance_on_end,
        story_mode,
        stderr,
    } = options;
    let decoder_options = DecoderOptions {
        stderr,
        ..Default::default()
    };
    let fps = std::env::var("TELEGA_VIDEO_FPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|value| (1..=30).contains(value))
        .unwrap_or(15);
    let decode_width = std::env::var("TELEGA_VIDEO_WIDTH")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|value| (160..=1920).contains(value))
        .unwrap_or(640);
    let mut displayed = preview
        .as_ref()
        .map(|image| show_memory_overlay(image, terminal))
        .transpose()?;
    let mut pipeline =
        spawn_video_pipeline_with(source.input(), 0.0, fps, decode_width, decoder_options)?;
    let mut paused = false;
    let mut segment_start = 0.0;
    let mut segment_frames = 0u64;
    let mut elapsed = 0.0;
    let mut controls = None;
    let mut controls_image = None;
    let mut input = VideoInput::default();
    let mut last_controls = None;
    let mut failure = None;
    let mut finished = false;
    let mut compatible = false;
    if story_mode {
        draw_story_action_bar(terminal, true)?;
    }

    let action = loop {
        let bounds = displayed
            .as_ref()
            .map(|image| (image.row, image.column, image.rows, image.columns))
            .unwrap_or_default();
        match video_command(terminal, bounds, controls, &mut input, story_mode)? {
            Some(VideoCommand::Close) => break ViewerAction::Close,
            Some(VideoCommand::Previous) => break ViewerAction::Previous,
            Some(VideoCommand::Next) => break ViewerAction::Next,
            Some(VideoCommand::React) => break ViewerAction::React,
            Some(VideoCommand::Reply) => break ViewerAction::Reply,
            Some(VideoCommand::TogglePause) => {
                if finished {
                    pipeline.stop();
                    pipeline = spawn_video_pipeline_with(
                        source.input(),
                        0.0,
                        fps,
                        decode_width,
                        DecoderOptions {
                            compatible,
                            ..decoder_options
                        },
                    )?;
                    segment_start = 0.0;
                    segment_frames = 0;
                    elapsed = 0.0;
                    finished = false;
                    failure = None;
                    paused = false;
                } else {
                    paused = !paused;
                    pipeline.set_paused(paused);
                }
            }
            Some(VideoCommand::Seek(ratio)) => {
                segment_start = (duration * ratio).clamp(0.0, (duration - 0.05).max(0.0));
                pipeline.stop();
                pipeline = spawn_video_pipeline_with(
                    source.input(),
                    segment_start,
                    fps,
                    decode_width,
                    DecoderOptions {
                        compatible,
                        ..decoder_options
                    },
                )?;
                finished = false;
                failure = None;
                paused = false;
                segment_frames = 0;
                elapsed = segment_start;
            }
            Some(VideoCommand::SeekPreview(ratio)) => {
                input.preview = Some(ratio);
                pipeline.set_paused(true);
            }
            None => {}
        }
        if !paused && !input.dragging {
            match pipeline.frames.recv_timeout(Duration::from_millis(16)) {
                Ok(Ok(frame)) => {
                    if !pipeline.audio_started {
                        pipeline.audio = spawn_audio_input(source.input(), segment_start).ok();
                        pipeline.audio_started = true;
                    }
                    segment_frames += 1;
                    elapsed = segment_start + segment_frames as f64 / f64::from(fps);
                    replace_video_frame(&mut displayed, frame, terminal)?;
                }
                result @ (Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected)) => {
                    let premature =
                        segment_frames == 0 || (duration > 0.0 && elapsed + 1.0 < duration);
                    if premature && !compatible {
                        compatible = true;
                        pipeline.stop();
                        segment_start = elapsed;
                        segment_frames = 0;
                        pipeline = spawn_video_pipeline_with(
                            source.input(),
                            segment_start,
                            fps,
                            decode_width,
                            DecoderOptions {
                                compatible: true,
                                ..decoder_options
                            },
                        )?;
                    } else {
                        if premature {
                            failure = Some(match result {
                                Ok(Err(error)) => error,
                                _ => "ffmpeg ended before the video finished".to_owned(),
                            });
                        }
                        // EOF holds the last frame and controls. Never close a viewer
                        // implicitly: the user can replay, seek or close it themselves.
                        pipeline.stop();
                        finished = true;
                        paused = true;
                        if advance_on_end && failure.is_none() {
                            break ViewerAction::Next;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else {
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        let shown = input
            .preview
            .map(|ratio| ratio * duration)
            .unwrap_or(elapsed);
        let (downloaded, total) = source.buffered();
        let key = (
            shown as u64,
            (shown / duration.max(1.0) * 36.0) as u32,
            downloaded
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0),
            paused,
            bounds,
        );
        if last_controls != Some(key) {
            controls = Some(draw_video_timeline(
                terminal,
                displayed.as_ref(),
                VideoProgress {
                    elapsed: shown,
                    duration,
                    downloaded: downloaded.min(total),
                    total,
                    paused,
                },
                &mut controls_image,
            )?);
            last_controls = Some(key);
        }
        tokio::task::yield_now().await;
    };
    pipeline.stop();
    if let Some(image) = displayed {
        image.delete(terminal)?;
    }
    if let Some(mut image) = controls_image {
        image.delete(&mut terminal.stdout)?;
    }
    windows.invalidate();
    if let Some(error) = failure {
        bail!("video playback failed: {error}");
    }
    Ok(action)
}

#[derive(Clone, Copy)]
enum VideoCommand {
    TogglePause,
    Seek(f64),
    SeekPreview(f64),
    Close,
    Previous,
    Next,
    React,
    Reply,
}

#[derive(Default)]
struct VideoInput {
    dragging: bool,
    preview: Option<f64>,
}

#[derive(Clone, Copy)]
struct VideoControls {
    row: u32,
    start: u32,
    width: u32,
    play_width: u32,
    timeline_start: u32,
    timeline_width: u32,
}

fn video_command(
    terminal: &mut Terminal,
    bounds: (u32, u32, u32, u32),
    controls: Option<VideoControls>,
    input: &mut VideoInput,
    story_mode: bool,
) -> Result<Option<VideoCommand>> {
    let key = terminal.get_key(0, false)?;
    let (row, column, rows, columns) = bounds;
    Ok(match key {
        27 => Some(VideoCommand::Close),
        value if story_mode && value == i32::from(b'r') => Some(VideoCommand::React),
        value if story_mode && value == i32::from(b'm') => Some(VideoCommand::Reply),
        KEY_LEFT => Some(VideoCommand::Previous),
        KEY_RIGHT => Some(VideoCommand::Next),
        value if value == i32::from(b' ') || value == i32::from(b'k') => {
            Some(VideoCommand::TogglePause)
        }
        MOUSE_EVENT => {
            let mouse = terminal.getmouse();
            let x = u32::from(mouse.x);
            let y = u32::from(mouse.y);
            if mouse.event == MouseEvent::LEFT
                && story_mode
                && let Some(action) = story_action_hit(terminal, x, y, true)
            {
                return Ok(Some(match action {
                    ViewerAction::React => VideoCommand::React,
                    ViewerAction::Reply => VideoCommand::Reply,
                    _ => unreachable!(),
                }));
            }
            if input.dragging {
                let ratio = controls
                    .map(|controls| {
                        f64::from(x.saturating_sub(controls.timeline_start))
                            / f64::from(controls.timeline_width.saturating_sub(1).max(1))
                    })
                    .unwrap_or_default()
                    .clamp(0.0, 1.0);
                input.preview = Some(ratio);
                if mouse.event == MouseEvent::IDLE {
                    input.dragging = false;
                    input.preview = None;
                    return Ok(Some(VideoCommand::Seek(ratio)));
                }
                return Ok(Some(VideoCommand::SeekPreview(ratio)));
            }
            let inside_image = rows > 0
                && columns > 0
                && x >= column
                && x < column.saturating_add(columns)
                && y >= row
                && y < row.saturating_add(rows);
            let inside_controls = controls.is_some_and(|controls| {
                y == controls.row
                    && x >= controls.start
                    && x < controls.start.saturating_add(controls.width)
            });
            let inside_play = controls.is_some_and(|controls| {
                y == controls.row
                    && x >= controls.start
                    && x < controls.start.saturating_add(controls.play_width)
            });
            let seek = controls.and_then(|controls| {
                (y == controls.row
                    && x >= controls.timeline_start
                    && x < controls
                        .timeline_start
                        .saturating_add(controls.timeline_width))
                .then(|| {
                    f64::from(x.saturating_sub(controls.timeline_start))
                        / f64::from(controls.timeline_width.max(1))
                })
            });
            match mouse.event {
                MouseEvent::LEFT if inside_play => Some(VideoCommand::TogglePause),
                MouseEvent::LEFT if seek.is_some() => {
                    input.dragging = true;
                    input.preview = seek;
                    Some(VideoCommand::SeekPreview(seek.unwrap()))
                }
                MouseEvent::LEFT if inside_image || inside_controls => None,
                MouseEvent::LEFT => Some(VideoCommand::Close),
                _ => None,
            }
        }
        _ => None,
    })
}

fn replace_video_frame(
    displayed: &mut Option<DisplayedImage>,
    frame: Vec<u8>,
    terminal: &mut Terminal,
) -> Result<()> {
    let (pixel_width, pixel_height) = png_dimensions(&frame)?;
    let (screen_height, screen_width) = terminal.getsize();
    let (columns, rows) = fit_dimensions(
        pixel_width,
        pixel_height,
        u32::from(screen_width),
        u32::from(screen_height).saturating_sub(2).max(1),
    );
    let row = u32::from(screen_height)
        .saturating_sub(2)
        .saturating_sub(rows)
        / 2;
    let column = u32::from(screen_width).saturating_sub(columns) / 2;
    let next_id = if displayed
        .as_ref()
        .is_some_and(|image| image.image.id() == 0x5447_0003)
    {
        0x5447_0004
    } else {
        0x5447_0003
    };
    let mut next = TerminalImage::new(next_id, ImageData::from_png_bytes(frame)?)?;
    let mut group = ImageGroup::begin(&mut terminal.stdout)?;
    write!(
        group,
        "\x1b[{};{}H",
        row.saturating_add(1),
        column.saturating_add(1)
    )?;
    next.upload(&mut group)?;
    place_overlay(&next, &mut group, columns, rows)?;
    if let Some(previous) = displayed.as_mut() {
        previous.image.delete(&mut group)?;
    }
    group.present()?;
    *displayed = Some(DisplayedImage {
        image: next,
        row,
        column,
        rows,
        columns,
    });
    Ok(())
}

struct VideoProgress {
    elapsed: f64,
    duration: f64,
    downloaded: u64,
    total: u64,
    paused: bool,
}

fn draw_video_timeline(
    terminal: &mut Terminal,
    image: Option<&DisplayedImage>,
    progress: VideoProgress,
    overlay: &mut Option<TerminalImage>,
) -> Result<VideoControls> {
    let VideoProgress {
        elapsed,
        duration,
        downloaded,
        total,
        paused,
    } = progress;
    let (height, width) = terminal.getsize();
    let bar_width = usize::from(width).saturating_sub(28).clamp(8, 36);
    let playback_ratio = if duration > 0.0 {
        (elapsed / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let played = (playback_ratio * bar_width as f64).round() as usize;
    let mut bar = String::with_capacity(bar_width);
    for index in 0..bar_width {
        bar.push(if index < played { '━' } else { '─' });
    }
    let buffered = downloaded
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0);
    let state = if paused { "[>]" } else { "[||]" };
    let text = format!(
        "{state}  {}  {bar}  {}  {buffered:>3}%",
        format_time(elapsed),
        format_time(duration),
    );
    let text_width = text.chars().count().min(usize::from(width));
    let start = usize::from(width).saturating_sub(text_width) / 2;
    let row = image
        .map(|image| {
            image
                .row
                .saturating_add(image.rows)
                .min(u32::from(height.saturating_sub(1)))
        })
        .unwrap_or_else(|| u32::from(height.saturating_sub(1)));
    let pixel_width = text_width.max(1) * 12;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{pixel_width}" height="28"><rect width="100%" height="100%" rx="8" fill="#202b36"/><text x="0" y="20" fill="#ffffff" font-size="20" font-family="monospace" textLength="{pixel_width}" lengthAdjust="spacingAndGlyphs">{}</text></svg>"##,
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    );
    let mut converter = Command::new("rsvg-convert")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("rsvg-convert is required for video controls")?;
    converter
        .stdin
        .take()
        .context("SVG renderer stdin unavailable")?
        .write_all(svg.as_bytes())?;
    let png = converter.wait_with_output()?;
    if !png.status.success() {
        bail!("cannot render video controls");
    }
    let id = if overlay
        .as_ref()
        .is_some_and(|image| image.id() == 0x5447_0020)
    {
        0x5447_0021
    } else {
        0x5447_0020
    };
    let mut next = TerminalImage::new(id, ImageData::from_png_bytes(png.stdout)?)?;
    let mut group = ImageGroup::begin(&mut terminal.stdout)?;
    write!(group, "\x1b[{};{}H", row + 1, start + 1)?;
    next.upload(&mut group)?;
    next.place_at_z(&mut group, text_width as u32, 1, i32::MAX)?;
    if let Some(previous) = overlay {
        previous.delete(&mut group)?;
    }
    group.present()?;
    *overlay = Some(next);
    Ok(VideoControls {
        row,
        start: start as u32,
        width: text_width as u32,
        play_width: state.len() as u32,
        timeline_start: start.saturating_add(state.len() + 9) as u32,
        timeline_width: bar_width as u32,
    })
}

fn format_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn set_child_paused(child: &Child, paused: bool) {
    let signal = if paused { libc::SIGSTOP } else { libc::SIGCONT };
    unsafe {
        libc::kill(child.id() as i32, signal);
    }
}

pub fn read_png_frame(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
    const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
    let mut signature = [0u8; 8];
    if let Err(error) = reader.read_exact(&mut signature[..1]) {
        if error.kind() == ErrorKind::UnexpectedEof {
            return Ok(None);
        }
        return Err(error.into());
    }
    reader.read_exact(&mut signature[1..])?;
    if &signature != b"\x89PNG\r\n\x1a\n" {
        bail!("ffmpeg produced an invalid PNG stream");
    }
    let mut frame = signature.to_vec();
    loop {
        let mut header = [0u8; 8];
        reader.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let size = frame
            .len()
            .checked_add(12)
            .and_then(|size| size.checked_add(length));
        if size.is_none_or(|size| size > MAX_FRAME_BYTES) {
            bail!("video frame is unexpectedly large");
        }
        frame.extend_from_slice(&header);
        let payload = frame.len();
        frame.resize(size.unwrap(), 0);
        reader.read_exact(&mut frame[payload..])?;
        if &header[4..8] == b"IEND" {
            return Ok(Some(frame));
        }
    }
}

pub fn spawn_audio_at(path: &Path, offset: f64) -> Result<Child> {
    spawn_audio_input(path.as_os_str(), offset)
}

fn spawn_audio_input(input: &OsStr, offset: f64) -> Result<Child> {
    let offset = format!("{:.3}", offset.max(0.0));
    let attempts = [
        (
            "mpv",
            vec![
                "--really-quiet".to_owned(),
                "--no-terminal".to_owned(),
                "--no-video".to_owned(),
                format!("--start={offset}"),
            ],
        ),
        (
            "ffplay",
            vec![
                "-nodisp".to_owned(),
                "-autoexit".to_owned(),
                "-loglevel".to_owned(),
                "quiet".to_owned(),
                "-ss".to_owned(),
                offset,
            ],
        ),
        ("afplay", Vec::new()),
    ];
    for (program, arguments) in attempts {
        match Command::new(program)
            .args(arguments)
            .arg(input)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("cannot start {program}")),
        }
    }
    bail!("audio playback requires mpv, ffplay, or afplay")
}

pub fn set_audio_paused(child: &Child, paused: bool) {
    set_child_paused(child, paused);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn png_stream_limits_the_whole_frame_and_rejects_partial_headers() {
        assert!(read_png_frame(&mut &b""[..]).unwrap().is_none());
        assert!(read_png_frame(&mut &b"\x89PNG"[..]).is_err());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(b"tEXt\0\0\0\0");
        png.extend_from_slice(&(64 * 1024 * 1024u32 - 24).to_be_bytes());
        png.extend_from_slice(b"IDAT");
        let error = read_png_frame(&mut png.as_slice()).unwrap_err();
        assert!(error.to_string().contains("unexpectedly large"));
    }

    #[test]
    fn decoder_seeks_end_index_mp4_and_stops_with_a_full_queue() {
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("telega-video-test-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("late-index.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=4",
                "-t",
                "3",
                "-c:v",
                "mpeg4",
                "-q:v",
                "3",
            ])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let input = path.as_os_str();
        let mut pipeline =
            spawn_video_pipeline_with(input, 0.0, 4, 64, Default::default()).unwrap();
        let first = pipeline
            .frames
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        thread::sleep(Duration::from_millis(900));
        let start = Instant::now();
        pipeline.stop();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "decoder shutdown blocked on its full frame queue"
        );
        let mut sought = spawn_video_pipeline_with(input, 1.5, 4, 64, Default::default()).unwrap();
        let frame = sought
            .frames
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(png_dimensions(&frame).unwrap(), (64, 48));
        assert_ne!(first, frame, "seeking should change the decoded frame");
        sought.stop();

        let mut compatible_seek = spawn_video_pipeline_with(
            input,
            2.5,
            4,
            64,
            DecoderOptions {
                compatible: true,
                ..Default::default()
            },
        )
        .unwrap();
        let sought_frame = compatible_seek.frames.recv_timeout(Duration::from_secs(2));
        compatible_seek.stop();
        assert!(
            sought_frame.unwrap().is_ok(),
            "compatible seeking must skip input without real-time pacing"
        );

        let mut looped = spawn_video_pipeline_with(
            input,
            0.0,
            4,
            64,
            DecoderOptions {
                round: true,
                looping: true,
                compatible: true,
                ..Default::default()
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut rounded = Vec::new();
        // The source has 12 frames. Receiving 16 proves the decoder loops
        // beyond EOF instead of playing just the first three seconds once.
        for _ in 0..16 {
            assert!(Instant::now() < deadline, "looping decoder stalled");
            rounded = looped
                .frames
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            assert_eq!(png_dimensions(&rounded).unwrap(), (64, 64));
        }
        looped.stop();
        let mut corner = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                "pipe:0",
                "-vf",
                "crop=1:1:0:0",
                "-frames:v",
                "1",
                "-pix_fmt",
                "rgba",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        corner.stdin.take().unwrap().write_all(&rounded).unwrap();
        let pixel = corner.wait_with_output().unwrap();
        assert!(pixel.status.success());
        assert_eq!(pixel.stdout.len(), 4);
        assert_eq!(
            pixel.stdout[3], 0,
            "round video corners must be transparent"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
