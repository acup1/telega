use std::io::{BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, fs::File};

use anyhow::{Context, Result, bail};
use grammers_client::Client;
use grammers_client::media::{Document, Downloadable, Media, Photo, PhotoSize};
use grammers_client::message::Message;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_session::types::PeerRef;
use terminalib::WindowManager;
use terminalib::terminal::Terminal;
use terminalib::tgp::{ImageData, ImageGroup, TerminalImage};
use terminalib::{KEY_LEFT, KEY_RIGHT, MOUSE_EVENT, MouseEvent};
use tokio::io::AsyncWriteExt;
use unicode_segmentation::UnicodeSegmentation;

use crate::telegram::Telegram;

const OVERLAY_Z_INDEX: i32 = i32::MAX - 1;
const GALLERY_PAGE: usize = 50;

#[derive(Clone)]
pub struct MemoryImage {
    png: Arc<Vec<u8>>,
    width: u32,
    height: u32,
}

pub struct AudioClip {
    pub path: PathBuf,
    pub duration: f64,
}

#[derive(Clone)]
pub struct StickerFrames {
    pub frames: Arc<Vec<MemoryImage>>,
    started: Instant,
}

impl StickerFrames {
    pub fn still(image: MemoryImage) -> Self {
        Self {
            frames: Arc::new(vec![image]),
            started: Instant::now(),
        }
    }
    pub fn frame_index(&self) -> usize {
        (self.started.elapsed().as_millis() / 100) as usize % self.frames.len().max(1)
    }
    pub fn current(&self) -> &MemoryImage {
        &self.frames[self.frame_index()]
    }
}

impl MemoryImage {
    pub(crate) fn identity(&self) -> usize {
        Arc::as_ptr(&self.png) as usize
    }
    pub fn from_png(png: Vec<u8>) -> Result<Self> {
        let (width, height) = png_dimensions(&png)?;
        Ok(Self {
            png: Arc::new(png),
            width,
            height,
        })
    }

    pub fn png(&self) -> &[u8] {
        self.png.as_slice()
    }

    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

pub struct ViewerContext<'a> {
    pub terminal: &'a mut Terminal,
    pub windows: &'a mut WindowManager,
    pub tgp_supported: bool,
    pub nerd_fonts: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Image,
    Video,
    Audio,
    Animation,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewerAction {
    Previous,
    Next,
    Close,
    React,
    Reply,
}

pub fn label(message: &Message) -> Option<String> {
    let media = effective_media(message)?;
    Some(match classify(&media) {
        Kind::Image | Kind::Video | Kind::Animation => return None,
        Kind::Audio => "🔊 audio / voice".to_owned(),
        Kind::File => file_label(match &media {
            Media::Document(document) => document.name(),
            _ => None,
        }),
    })
}

fn file_label(name: Option<&str>) -> String {
    let name = name
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let name: String = UnicodeSegmentation::graphemes(name, true)
        .filter(|grapheme| {
            !grapheme.chars().any(|c| {
                c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            })
        })
        .take(80)
        .collect();
    if name.trim().is_empty() {
        "📎 file".to_owned()
    } else {
        format!("📎 {name}")
    }
}

pub fn has_inline_preview(message: &Message) -> bool {
    let Some(media) = effective_media(message) else {
        return false;
    };
    matches!(
        classify(&media),
        Kind::Image | Kind::Video | Kind::Animation
    )
}

pub fn is_audio(message: &Message) -> bool {
    effective_media(message).is_some_and(|media| classify(&media) == Kind::Audio)
}

pub fn is_circle(message: &Message) -> bool {
    matches!(message.media(), Some(Media::Document(document)) if document.raw.document.as_ref().is_some_and(|raw| {
        matches!(raw, tl::enums::Document::Document(doc) if doc.attributes.iter().any(|attribute| matches!(attribute, tl::enums::DocumentAttribute::Video(video) if video.round_message)))
    }))
}

pub fn is_gif(message: &Message) -> bool {
    !is_sticker(message)
        && matches!(message.media(), Some(Media::Document(document)) if document.mime_type() == Some("image/gif") || document.raw.document.as_ref().is_some_and(|raw| {
            matches!(raw, tl::enums::Document::Document(doc) if doc.attributes.iter().any(|attribute| matches!(attribute, tl::enums::DocumentAttribute::Animated)))
        }))
}

pub fn is_sticker(message: &Message) -> bool {
    match message.media() {
        Some(Media::Sticker(_)) => true,
        Some(Media::Document(document)) => document.mime_type() == Some("application/x-tgsticker")
            || document.raw.document.as_ref().is_some_and(|raw| {
                matches!(raw, tl::enums::Document::Document(doc) if doc.attributes.iter().any(|a| {
                    matches!(a, tl::enums::DocumentAttribute::Sticker(_) | tl::enums::DocumentAttribute::CustomEmoji(_))
                }))
            }),
        _ => false,
    }
}

pub fn sticker_set(message: &Message) -> Option<tl::enums::InputStickerSet> {
    sticker_set_from_media(&message.media()?)
}

fn sticker_set_from_media(media: &Media) -> Option<tl::enums::InputStickerSet> {
    let set = match media {
        Media::Sticker(sticker) => Some(sticker.raw_attrs.stickerset.clone()),
        Media::Document(document) => match document.raw.document.as_ref()? {
            tl::enums::Document::Document(document) => {
                document.attributes.iter().find_map(|a| match a {
                    tl::enums::DocumentAttribute::Sticker(sticker) => {
                        Some(sticker.stickerset.clone())
                    }
                    tl::enums::DocumentAttribute::CustomEmoji(emoji) => {
                        Some(emoji.stickerset.clone())
                    }
                    _ => None,
                })
            }
            _ => None,
        },
        _ => None,
    }?;
    (!matches!(set, tl::enums::InputStickerSet::Empty)).then_some(set)
}

pub async fn prepare_sticker(
    client: Client,
    message: Message,
    cache: PathBuf,
    first_frame: impl FnOnce(MemoryImage) + Send + 'static,
) -> Result<StickerFrames> {
    fs::create_dir_all(&cache)?;
    let media = effective_media(&message).context("sticker is unavailable")?;
    let (extension, kind) = describe(&media);
    let path = media_path(&cache, &message, extension);
    prepare_sticker_asset(client, media, path, extension, kind, first_frame).await
}

pub fn sticker_document(raw: tl::enums::Document) -> Document {
    Document::from_raw_media(tl::types::MessageMediaDocument {
        nopremium: false,
        spoiler: false,
        video: false,
        round: false,
        voice: false,
        document: Some(raw),
        alt_documents: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    })
}

pub async fn prepare_picker_sticker(
    client: Client,
    raw: tl::enums::Document,
    cache: PathBuf,
) -> Result<StickerFrames> {
    prepare_picker_sticker_progressive(client, raw, cache, |_| {}).await
}

pub async fn prepare_picker_sticker_progressive(
    client: Client,
    raw: tl::enums::Document,
    cache: PathBuf,
    first_frame: impl FnOnce(MemoryImage) + Send + 'static,
) -> Result<StickerFrames> {
    fs::create_dir_all(&cache)?;
    let document = sticker_document(raw);
    let extension = extension_for(document.mime_type().unwrap_or("image/webp"));
    let kind = if matches!(extension, ".tgs" | ".webm" | ".mp4" | ".gif") {
        Kind::Animation
    } else {
        Kind::Image
    };
    let path = cache.join(format!("picker-{}{}", document.id(), extension));
    prepare_sticker_asset(
        client,
        Media::Document(document),
        path,
        extension,
        kind,
        first_frame,
    )
    .await
}

async fn prepare_sticker_asset(
    client: Client,
    media: Media,
    path: PathBuf,
    extension: &'static str,
    kind: Kind,
    first_frame: impl FnOnce(MemoryImage) + Send + 'static,
) -> Result<StickerFrames> {
    let temporary = TemporaryMedia::new(&path);
    download_if_needed(&client, &media, temporary.path()).await?;
    let result = tokio::task::spawn_blocking(move || {
        decode_sticker_frames(temporary.path(), extension, kind, first_frame)
    })
    .await
    .context("sticker decoder stopped")?;
    let frames = result?;
    if frames.is_empty() {
        bail!("sticker has no frames");
    }
    Ok(StickerFrames {
        frames: Arc::new(frames),
        started: Instant::now(),
    })
}

fn decode_sticker_frames(
    path: &Path,
    extension: &str,
    kind: Kind,
    first_frame: impl FnOnce(MemoryImage),
) -> Result<Vec<MemoryImage>> {
    if kind != Kind::Animation {
        let frame = memory_image(&ensure_png(path)?)?;
        first_frame(frame.clone());
        return Ok(vec![frame]);
    }
    let mut command = if extension == ".tgs" {
        tgs_command(path)
    } else {
        let mut command = Command::new("ffmpeg");
        command.args(["-v", "error"]);
        // Native VP9 decoding discards the alpha plane of Telegram WEBM stickers.
        if extension == ".webm" {
            command.args(["-c:v", "libvpx-vp9"]);
        }
        command.arg("-i").arg(path).args([
            "-an",
            "-vf",
            "fps=10,scale=256:256:force_original_aspect_ratio=decrease:force_divisible_by=2,format=rgba",
            "-frames:v",
            "60",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ]);
        command
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(crate::child_stderr())
        .spawn()
        .context("cannot start sticker decoder (python-lottie/rsvg-convert or ffmpeg required)")?;
    let mut stream = BufReader::new(
        child
            .stdout
            .take()
            .context("sticker decoder has no stdout")?,
    );
    let mut first_frame = Some(first_frame);
    let mut frames = Vec::new();
    let decoded = (|| -> Result<()> {
        while let Some(png) = read_png_frame(&mut stream)? {
            let frame = MemoryImage::from_png(png)?;
            if let Some(callback) = first_frame.take() {
                callback(frame.clone());
            }
            frames.push(frame);
            if frames.len() >= 60 {
                break;
            }
        }
        Ok(())
    })();
    if decoded.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    decoded?;
    if !status.success() && frames.is_empty() {
        if extension == ".webm" {
            return decode_sticker_frames(
                path,
                ".video",
                kind,
                first_frame.context("missing sticker callback")?,
            );
        }
        bail!(
            "sticker decoder failed for {} ({status}); see log",
            path.display()
        );
    }
    anyhow::ensure!(!frames.is_empty(), "sticker decoder produced no frames");
    Ok(frames)
}

pub fn audio_duration(message: &Message) -> f64 {
    effective_media(message)
        .as_ref()
        .and_then(media_duration)
        .unwrap_or_default()
}

pub fn has_custom_emoji(message: &Message) -> bool {
    !crate::custom_emoji::entities(message).is_empty()
}

pub async fn prepare_custom_emoji(
    client: Client,
    message: Message,
    cache: PathBuf,
) -> Result<Vec<Option<StickerFrames>>> {
    let ids: Vec<_> = crate::custom_emoji::entities(&message)
        .into_iter()
        .map(|(_, _, id)| id)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let cache = cache.join(format!("emoji-{}", message.id()));
    fs::create_dir_all(&cache)?;
    let documents = client
        .invoke(&tl::functions::messages::GetCustomEmojiDocuments {
            document_id: ids.clone(),
        })
        .await?;
    let mut rendered = std::collections::HashMap::new();
    for raw in documents {
        let id = match &raw {
            tl::enums::Document::Document(document) => document.id,
            _ => continue,
        };
        if let Ok(frames) = prepare_picker_sticker(client.clone(), raw, cache.clone()).await {
            rendered.insert(id, frames);
        }
    }
    if rendered.is_empty() {
        bail!("custom emoji could not be rendered");
    }
    Ok(ids
        .into_iter()
        .map(|id| rendered.get(&id).cloned())
        .collect())
}

pub async fn prepare_audio(client: Client, message: Message, cache: PathBuf) -> Result<AudioClip> {
    fs::create_dir_all(&cache)?;
    let media = effective_media(&message).context("this message has no media")?;
    if classify(&media) != Kind::Audio && !is_circle(&message) {
        bail!("this message is not audio");
    }
    let (extension, _) = describe(&media);
    let path = media_path(&cache, &message, extension);
    download_if_needed(&client, &media, &path).await?;
    Ok(AudioClip {
        path,
        duration: media_duration(&media).unwrap_or_default(),
    })
}

pub async fn prepare_reaction_emoji(
    client: Client,
    id: i64,
    cache: PathBuf,
    graphics: bool,
) -> Result<(Option<StickerFrames>, String)> {
    let documents = client
        .invoke(&tl::functions::messages::GetCustomEmojiDocuments {
            document_id: vec![id],
        })
        .await?;
    let raw = documents
        .into_iter()
        .next()
        .context("custom reaction emoji is unavailable")?;
    let alt = match &raw {
        tl::enums::Document::Document(doc) => doc
            .attributes
            .iter()
            .find_map(|attribute| match attribute {
                tl::enums::DocumentAttribute::CustomEmoji(emoji) => Some(emoji.alt.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "?".to_owned()),
        _ => "?".to_owned(),
    };
    if !graphics {
        return Ok((None, alt));
    }
    let frames = prepare_picker_sticker(client, raw, cache.join("reactions")).await?;
    Ok((Some(frames), alt))
}

pub fn opens_in_gallery(message: &Message) -> bool {
    if is_sticker(message) {
        return false;
    }
    effective_media(message).is_some_and(|media| {
        let (extension, kind) = describe(&media);
        kind == Kind::Image || extension == ".tgs"
    })
}

pub async fn prepare_inline_preview(
    client: Client,
    message: Message,
    cache: PathBuf,
) -> Result<MemoryImage> {
    fs::create_dir_all(&cache)?;
    let media = effective_media(&message).context("this message has no media")?;
    let (extension, _) = describe(&media);
    if extension == ".tgs" {
        let temporary = TemporaryMedia::new(&media_path(&cache, &message, extension));
        download_if_needed(&client, &media, temporary.path()).await?;
        return tokio::task::spawn_blocking(move || {
            memory_image(&ensure_tgs_preview(temporary.path())?)
        })
        .await
        .context("preview worker stopped")?;
    }

    let Some(thumbnail) = largest_thumbnail(&media) else {
        if matches!(media, Media::Sticker(_)) {
            let temporary = TemporaryMedia::new(&media_path(&cache, &message, extension));
            download_if_needed(&client, &media, temporary.path()).await?;
            return tokio::task::spawn_blocking(move || {
                memory_image(&ensure_png(temporary.path())?)
            })
            .await
            .context("preview worker stopped")?;
        }
        return Err(anyhow::anyhow!("media has no Telegram thumbnail"));
    };
    let temporary = TemporaryMedia::new(&cache.join(format!(
        "{}-{:+}-thumbnail.jpg",
        message.peer_id(),
        message.id()
    )));
    download_atomic(&client, &thumbnail, temporary.path()).await?;
    tokio::task::spawn_blocking(move || memory_image(&ensure_png(temporary.path())?))
        .await
        .context("preview worker stopped")?
}

pub(crate) async fn prepare_media_thumbnail(
    client: Client,
    media: Media,
    path: PathBuf,
) -> Result<MemoryImage> {
    let thumbnail = largest_thumbnail(&media).context("media has no Telegram thumbnail")?;
    let temporary = TemporaryMedia::new(&path);
    download_atomic(&client, &thumbnail, temporary.path()).await?;
    tokio::task::spawn_blocking(move || memory_image(&ensure_png(temporary.path())?))
        .await
        .context("thumbnail converter stopped")?
}

pub struct Avatar {
    pub square: PathBuf,
    pub round: PathBuf,
}

pub async fn prepare_avatar(client: Client, peer: Peer, cache: PathBuf) -> Result<Option<Avatar>> {
    let Some(square) = prepare_avatar_size(client, peer, cache, false).await? else {
        return Ok(None);
    };
    let source = square.clone();
    let round = tokio::task::spawn_blocking(move || ensure_round_avatar(&source))
        .await
        .context("round avatar worker stopped")??;
    Ok(Some(Avatar { square, round }))
}

pub async fn prepare_avatar_size(
    client: Client,
    peer: Peer,
    cache: PathBuf,
    large: bool,
) -> Result<Option<PathBuf>> {
    let Some(photo) = peer.photo(large).await? else {
        return Ok(None);
    };
    let id = peer.id().bot_api_dialog_id().unwrap_or_default();
    let photo_id = match &photo.raw {
        tl::enums::InputFileLocation::InputPeerPhotoFileLocation(location) => location.photo_id,
        _ => 0,
    };
    let suffix = if large { "-large" } else { "" };
    let path = cache.join(format!("avatar-{id}-{photo_id}{suffix}.jpg"));
    if !path.exists() {
        download_atomic(&client, &photo, &path).await?;
    }
    let worker_path = path.clone();
    let converted = tokio::task::spawn_blocking(move || ensure_png(&worker_path))
        .await
        .context("avatar preview worker stopped")?;
    let png = match converted {
        Ok(png) => png,
        Err(_) => {
            // Repair partial avatars left by an interrupted older client.
            cleanup_generated(&path);
            download_atomic(&client, &photo, &path).await?;
            tokio::task::spawn_blocking(move || ensure_png(&path))
                .await
                .context("avatar preview worker stopped")??
        }
    };
    // The current photo can be a contact-specific avatar. Versioning the cache
    // by photo ID keeps an older server avatar from masking a local override.
    if let Ok(entries) = fs::read_dir(&cache) {
        let prefix = format!("avatar-{id}-");
        let current = format!("avatar-{id}-{photo_id}");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(&prefix) && !name.starts_with(&current) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(Some(png))
}

fn ensure_round_avatar(path: &Path) -> Result<PathBuf> {
    // A separate versioned derivative also upgrades already cached square
    // previews. Keep the original photo intact for the full-screen gallery.
    let rounded = path.with_extension("round-v1.png");
    if image_dimensions(&rounded).is_ok_and(|(w, h)| w == h && w <= 256) {
        return Ok(rounded);
    }
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-vf",
            "format=rgba,crop='min(iw,ih)':'min(iw,ih)',scale='min(iw,256)':-1:flags=lanczos,setsar=1,geq=r='r(X,Y)':g='g(X,Y)':b='b(X,Y)':a='alpha(X,Y)*clip(W/2-hypot(X+0.5-W/2,Y+0.5-H/2)+0.5,0,1)',format=rgba",
            "-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "pipe:1",
        ])
        .stderr(Stdio::null())
        .output()
        .context("ffmpeg is required to prepare round avatars")?;
    if !output.status.success() {
        bail!("ffmpeg could not prepare avatar {}", path.display());
    }
    let image = MemoryImage::from_png(output.stdout)?;
    let (width, height) = image.dimensions();
    if width != height || width > 256 {
        bail!("invalid rounded avatar dimensions");
    }
    // Publish complete PNGs atomically, even if a refresh overlaps a worker.
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let temporary = rounded.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    let result = fs::write(&temporary, image.png()).and_then(|()| fs::rename(&temporary, &rounded));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(rounded)
}

pub async fn open(
    client: &Client,
    message: &Message,
    cache: &Path,
    preview: Option<MemoryImage>,
    viewer: ViewerContext<'_>,
) -> Result<String> {
    let ViewerContext {
        terminal,
        windows,
        tgp_supported,
        nerd_fonts,
    } = viewer;
    fs::create_dir_all(cache)?;
    let media = effective_media(message).context("this message has no media")?;
    let (extension, kind) = describe(&media);
    let path = media_path(cache, message, extension);

    if !tgp_supported && matches!(kind, Kind::Image | Kind::Animation) {
        let _ = show_unsupported(terminal, windows, nerd_fonts)?;
        return Ok("TGP is not supported by this terminal".to_owned());
    }

    if matches!(kind, Kind::Video | Kind::Animation) && extension != ".tgs" && tgp_supported {
        let result = play_video_stream_tgp(
            client.clone(),
            media,
            preview,
            terminal,
            windows,
            false,
            false,
        )
        .await;
        cleanup_generated(&path);
        return result.map(|_| "Played streaming media".to_owned());
    }

    if kind == Kind::Image && tgp_supported {
        let result = show_progressive_image(
            client.clone(),
            media,
            path.clone(),
            (preview, Some(message.clone())),
            terminal,
            windows,
            false,
        )
        .await;
        cleanup_generated(&path);
        return result.map(|_| "Opened image".to_owned());
    }

    match wait_media_load(download_if_needed(client, &media, &path), terminal).await {
        Ok(Some(())) => {}
        Ok(None) => {
            cleanup_generated(&path);
            return Ok("Media closed".to_owned());
        }
        Err(error) => {
            cleanup_generated(&path);
            return Err(error);
        }
    }
    let result = (|| -> Result<String> {
        if extension == ".tgs" {
            if !tgp_supported {
                let _ = show_unsupported(terminal, windows, nerd_fonts)?;
                return Ok("TGP is not supported by this terminal".to_owned());
            }
            let frames = ensure_tgs_frames(&path)?;
            let _ = show_tgp_animation(&frames, terminal, windows)?;
            return Ok("Opened animated media".to_owned());
        }

        match kind {
            Kind::Image => {
                if !tgp_supported {
                    let _ = show_unsupported(terminal, windows, nerd_fonts)?;
                    return Ok("TGP is not supported by this terminal".to_owned());
                }
                let png = ensure_png(&path)?;
                let _ = show_tgp(&png, terminal, windows, false)?;
                Ok("Opened image".to_owned())
            }
            Kind::Video | Kind::Animation => {
                play_audio(&path, terminal)?;
                Ok("Played media".to_owned())
            }
            Kind::Audio => {
                play_audio(&path, terminal)?;
                Ok("Played audio".to_owned())
            }
            Kind::File => Ok("Attachment fetched temporarily".to_owned()),
        }
    })();
    cleanup_generated(&path);
    result
}

pub async fn open_gallery(
    client: &Client,
    peer: PeerRef,
    messages: &[Message],
    start_index: usize,
    cache: &Path,
    initial_preview: Option<MemoryImage>,
    viewer: ViewerContext<'_>,
) -> Result<i32> {
    fs::create_dir_all(cache)?;
    let ViewerContext {
        terminal,
        windows,
        tgp_supported,
        nerd_fonts,
    } = viewer;
    let start_id = messages
        .get(start_index)
        .map(Message::id)
        .context("selected message no longer exists")?;
    let mut gallery: Vec<_> = messages
        .iter()
        .filter(|message| opens_in_gallery(message))
        .cloned()
        .collect();
    gallery.sort_by_key(Message::id);
    gallery.dedup_by_key(|message| message.id());
    let mut position = gallery
        .iter()
        .position(|message| message.id() == start_id)
        .context("selected message is not an image")?;
    let mut oldest_exhausted = false;
    let mut newest_exhausted = false;
    let mut initial_preview = initial_preview;

    loop {
        let message = &gallery[position];
        let action = if tgp_supported {
            let media = effective_media(message).context("this message has no media")?;
            let (extension, _) = describe(&media);
            let path = media_path(cache, message, extension);
            let preview = (message.id() == start_id)
                .then(|| initial_preview.take())
                .flatten();
            let result = if extension == ".tgs" {
                if let Err(error) = download_if_needed(client, &media, &path).await {
                    cleanup_generated(&path);
                    return Err(error);
                }
                ensure_tgs_frames(&path)
                    .and_then(|frames| show_tgp_animation(&frames, terminal, windows))
            } else {
                show_progressive_image(
                    client.clone(),
                    media,
                    path.clone(),
                    (preview, Some(message.clone())),
                    terminal,
                    windows,
                    false,
                )
                .await
            };
            cleanup_generated(&path);
            result?
        } else {
            show_unsupported(terminal, windows, nerd_fonts)?
        };
        match action {
            ViewerAction::Previous if position > 0 => position -= 1,
            ViewerAction::Previous if !oldest_exhausted => {
                let mut cursor = gallery.first().map(Message::id).unwrap_or(start_id);
                loop {
                    let Some(page) = wait_media_load(
                        Telegram::messages_before(client, peer, GALLERY_PAGE, cursor),
                        terminal,
                    )
                    .await?
                    else {
                        return Ok(gallery[position].id());
                    };
                    if page.is_empty() {
                        oldest_exhausted = true;
                        break;
                    }
                    cursor = page.first().map(Message::id).unwrap_or(cursor);
                    let page_is_short = page.len() < GALLERY_PAGE;
                    let older: Vec<_> = page.into_iter().filter(opens_in_gallery).collect();
                    if !older.is_empty() {
                        let added = older.len();
                        gallery.splice(0..0, older);
                        position = added - 1;
                        break;
                    }
                    if page_is_short {
                        oldest_exhausted = true;
                        break;
                    }
                }
            }
            ViewerAction::Next if position + 1 < gallery.len() => position += 1,
            ViewerAction::Next if !newest_exhausted => {
                let mut cursor = gallery.last().map(Message::id).unwrap_or(start_id);
                loop {
                    let Some(page) = wait_media_load(
                        Telegram::messages_after(client, peer, GALLERY_PAGE, cursor),
                        terminal,
                    )
                    .await?
                    else {
                        return Ok(gallery[position].id());
                    };
                    if page.is_empty() {
                        newest_exhausted = true;
                        break;
                    }
                    cursor = page.last().map(Message::id).unwrap_or(cursor);
                    let page_is_short = page.len() < GALLERY_PAGE;
                    let newer: Vec<_> = page.into_iter().filter(opens_in_gallery).collect();
                    if !newer.is_empty() {
                        gallery.extend(newer);
                        position += 1;
                        break;
                    }
                    if page_is_short {
                        newest_exhausted = true;
                        break;
                    }
                }
            }
            ViewerAction::Close => return Ok(gallery[position].id()),
            _ => {}
        }
    }
}

/// Open the current peer's available stories without joining a channel.
pub struct StoryViewResult {
    pub count: usize,
    pub viewed: Vec<i32>,
}

#[derive(Clone, Copy)]
pub enum StorySource {
    Active,
    Pinned,
}

pub async fn open_stories(
    client: &Client,
    peer: PeerRef,
    cache: &Path,
    viewer: ViewerContext<'_>,
    source: StorySource,
    start_id: Option<i32>,
) -> Result<StoryViewResult> {
    let ViewerContext {
        terminal,
        windows,
        tgp_supported,
        nerd_fonts,
    } = viewer;
    if !tgp_supported {
        let _ = show_unsupported(terminal, windows, nerd_fonts)?;
        return Ok(StoryViewResult {
            count: 0,
            viewed: Vec::new(),
        });
    }
    let (mut stories, mut has_more) = match source {
        StorySource::Active => {
            let Some(response) = wait_media_load(
                async {
                    Ok(client
                        .invoke(&tl::functions::stories::GetPeerStories { peer: peer.into() })
                        .await?)
                },
                terminal,
            )
            .await?
            else {
                return Ok(StoryViewResult {
                    count: 0,
                    viewed: Vec::new(),
                });
            };
            let tl::enums::stories::PeerStories::Stories(response) = response;
            let tl::enums::PeerStories::Stories(peer_stories) = response.stories;
            (story_items(peer_stories.stories), false)
        }
        StorySource::Pinned => {
            let Some(page) = pinned_story_page(client, peer, 0, terminal).await? else {
                return Ok(StoryViewResult {
                    count: 0,
                    viewed: Vec::new(),
                });
            };
            page
        }
    };
    if stories.is_empty() {
        return Ok(StoryViewResult {
            count: 0,
            viewed: Vec::new(),
        });
    }
    fs::create_dir_all(cache)?;
    if let Some(start_id) = start_id {
        while !stories.iter().any(|(id, _)| *id == start_id) && has_more {
            let offset = stories.last().map(|(id, _)| *id).unwrap_or_default();
            let Some((page, more)) = pinned_story_page(client, peer, offset, terminal).await?
            else {
                break;
            };
            let old_len = stories.len();
            let known: std::collections::HashSet<_> = stories.iter().map(|(id, _)| *id).collect();
            stories.extend(page.into_iter().filter(|(id, _)| !known.contains(id)));
            has_more = more && stories.len() > old_len;
        }
    }
    let mut index = start_id
        .and_then(|id| stories.iter().position(|(story_id, _)| *story_id == id))
        .unwrap_or(0);
    let mut viewed = Vec::new();
    let mut known = std::collections::HashSet::new();
    loop {
        let (id, media) = &stories[index];
        let (extension, kind) = describe(media);
        let path = cache.join(format!("story-{id}{extension}"));
        let action: Result<ViewerAction> = match kind {
            Kind::Image => {
                show_progressive_image(
                    client.clone(),
                    media.clone(),
                    path.clone(),
                    (None, None),
                    terminal,
                    windows,
                    true,
                )
                .await
            }
            Kind::Video | Kind::Animation => {
                let thumbnail = prepare_media_thumbnail(
                    client.clone(),
                    media.clone(),
                    path.with_extension("thumbnail.jpg"),
                )
                .await
                .ok();
                play_video_stream_tgp(
                    client.clone(),
                    media.clone(),
                    thumbnail,
                    terminal,
                    windows,
                    true,
                    true,
                )
                .await
            }
            _ => Ok(ViewerAction::Next),
        };
        cleanup_generated(&path);
        let action = action?;
        if known.insert(*id) {
            viewed.push(*id);
        }
        match action {
            ViewerAction::Previous if index > 0 => index -= 1,
            ViewerAction::React => {
                if let Some(reaction) = choose_story_reaction(terminal)? {
                    client
                        .invoke(&tl::functions::stories::SendReaction {
                            add_to_recent: true,
                            peer: peer.into(),
                            story_id: *id,
                            reaction,
                        })
                        .await?;
                }
            }
            ViewerAction::Reply => {
                if let Some(reply) = prompt_story_reply(terminal)? {
                    send_story_reply(client, peer, *id, reply).await?;
                }
            }
            ViewerAction::Next => {
                if index + 1 == stories.len() && has_more {
                    let offset = stories.last().map(|(id, _)| *id).unwrap_or_default();
                    if let Some((page, more)) =
                        pinned_story_page(client, peer, offset, terminal).await?
                    {
                        let old_len = stories.len();
                        let known_ids: std::collections::HashSet<_> =
                            stories.iter().map(|(id, _)| *id).collect();
                        stories.extend(page.into_iter().filter(|(id, _)| !known_ids.contains(id)));
                        has_more = more && stories.len() > old_len;
                    } else {
                        has_more = false;
                    }
                }
                if index + 1 < stories.len() {
                    index += 1;
                } else {
                    break;
                }
            }
            ViewerAction::Close | ViewerAction::Previous => break,
        }
    }
    Ok(StoryViewResult {
        count: stories.len(),
        viewed,
    })
}

fn story_items(items: Vec<tl::enums::StoryItem>) -> Vec<(i32, Media)> {
    items
        .into_iter()
        .filter_map(|item| {
            let tl::enums::StoryItem::Item(item) = item else {
                return None;
            };
            Media::from_raw(item.media).map(|media| (item.id, media))
        })
        .collect()
}

async fn pinned_story_page(
    client: &Client,
    peer: PeerRef,
    offset: i32,
    terminal: &mut Terminal,
) -> Result<Option<(Vec<(i32, Media)>, bool)>> {
    let response = wait_media_load(
        async {
            Ok(client
                .invoke(&tl::functions::stories::GetPinnedStories {
                    peer: peer.into(),
                    offset_id: offset,
                    limit: 30,
                })
                .await?)
        },
        terminal,
    )
    .await?;
    let Some(tl::enums::stories::Stories::Stories(response)) = response else {
        return Ok(None);
    };
    let count = response.count.max(0) as usize;
    let items = story_items(response.stories);
    let has_more = items.len() >= 30 && count > items.len();
    Ok(Some((items, has_more)))
}

// Keep Escape responsive while fetching a gallery page or a media file. Dropping
// the future cancels the pending download; callers still clean their own files.
/// Profile photos are fetched lazily by the MTProto iterator, not from chat history.
pub async fn open_avatar_gallery(
    client: &Client,
    peer: PeerRef,
    cache: &Path,
    current_photo: Option<PathBuf>,
    viewer: ViewerContext<'_>,
) -> Result<()> {
    let ViewerContext {
        terminal,
        windows,
        tgp_supported,
        nerd_fonts,
    } = viewer;
    if !tgp_supported {
        let _ = show_unsupported(terminal, windows, nerd_fonts)?;
        return Ok(());
    }
    fs::create_dir_all(cache)?;
    let mut iterator = client.iter_profile_photos(peer);
    let mut photos = Vec::new();
    let mut position = 0usize;
    let mut exhausted = false;
    let mut showing_current = current_photo.is_some();
    loop {
        if showing_current {
            let path = current_photo.as_ref().expect("current avatar exists");
            match show_tgp(path, terminal, windows, false)? {
                ViewerAction::Close => return Ok(()),
                ViewerAction::Previous => continue,
                ViewerAction::Next => showing_current = false,
                ViewerAction::React | ViewerAction::Reply => continue,
            }
            continue;
        }
        if position == photos.len() {
            let Some(next) =
                wait_media_load(async { Ok(iterator.next().await?) }, terminal).await?
            else {
                return Ok(());
            };
            match next {
                Some(photo) => photos.push(photo),
                None if photos.is_empty() => return Ok(()),
                None => {
                    position = photos.len() - 1;
                    exhausted = true;
                }
            }
        }
        let photo = &photos[position];
        let path = cache.join(format!(
            "avatar-history-{}-{}.jpg",
            peer.id.bot_api_dialog_id().unwrap_or_default(),
            photo.id()
        ));
        let action = show_progressive_image(
            client.clone(),
            Media::Photo(photo.clone()),
            path,
            (None, None),
            terminal,
            windows,
            false,
        )
        .await?;
        match action {
            ViewerAction::Close => return Ok(()),
            ViewerAction::Previous if position == 0 && current_photo.is_some() => {
                showing_current = true
            }
            ViewerAction::Previous => position = position.saturating_sub(1),
            ViewerAction::Next if position + 1 < photos.len() || !exhausted => position += 1,
            ViewerAction::Next => {}
            ViewerAction::React | ViewerAction::Reply => {}
        }
    }
}

async fn wait_media_load<T>(
    load: impl std::future::Future<Output = Result<T>>,
    terminal: &mut Terminal,
) -> Result<Option<T>> {
    tokio::pin!(load);
    loop {
        tokio::select! {
            result = &mut load => return result.map(Some),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                if terminal.get_key(0, false)? == 27 {
                    return Ok(None);
                }
            }
        }
    }
}

fn largest_thumbnail(media: &Media) -> Option<PhotoSize> {
    let thumbnails = match media {
        Media::Photo(photo) => photo.thumbs(),
        Media::Document(document) => document.thumbs(),
        Media::Sticker(sticker) => sticker.document.thumbs(),
        _ => Vec::new(),
    };
    thumbnails.into_iter().max_by_key(PhotoSize::size)
}

fn show_unsupported(
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    nerd_fonts: bool,
) -> Result<ViewerAction> {
    let (height, width) = terminal.getsize();
    let row = u32::from(height) / 2;
    let column = u32::from(width) / 2;
    terminal.setpos(row as u16, column as u16);
    write!(
        terminal.stdout,
        "{}",
        if nerd_fonts { "\u{f03e}" } else { "!" }
    )?;
    terminal.refresh()?;
    let action = wait_for_viewer_action(
        terminal,
        row,
        column,
        1,
        if nerd_fonts { 2 } else { 1 },
        false,
    )?;
    terminal.setpos(row as u16, column as u16);
    write!(terminal.stdout, "  ")?;
    terminal.refresh()?;
    windows.invalidate();
    Ok(action)
}

fn media_path(cache: &Path, message: &Message, extension: &str) -> PathBuf {
    cache.join(format!(
        "{}-{:+}{}",
        message.peer_id(),
        message.id(),
        extension
    ))
}

fn memory_image(path: &Path) -> Result<MemoryImage> {
    let png = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let (width, height) = png_dimensions(&png)
        .with_context(|| format!("cannot read PNG dimensions from {}", path.display()))?;
    Ok(MemoryImage {
        png: Arc::new(png),
        width,
        height,
    })
}

fn unique_temporary_path(path: &Path, suffix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    path.with_extension(format!(
        "{}-{}-{suffix}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        path.extension().unwrap_or_default().to_string_lossy(),
    ))
}

/// Ownership travels into the blocking converter: aborting its async caller
/// neither removes an active decoder's input nor leaks its output afterward.
struct TemporaryMedia(PathBuf);

impl TemporaryMedia {
    fn new(path: &Path) -> Self {
        Self(unique_temporary_path(path, "media"))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryMedia {
    fn drop(&mut self) {
        cleanup_generated(&self.0);
    }
}

struct PendingDownload(PathBuf);

impl Drop for PendingDownload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn cleanup_generated(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("preview.png"));
    if !path.extension().is_some_and(|extension| extension == "tgs") {
        return;
    }
    let stem = path.with_extension("");
    for index in 0..60 {
        let _ = fs::remove_file(PathBuf::from(format!(
            "{}-preview-{index:02}.svg",
            stem.display()
        )));
        let _ = fs::remove_file(PathBuf::from(format!(
            "{}-preview-{index:02}.png",
            stem.display()
        )));
    }
}

fn effective_media(message: &Message) -> Option<Media> {
    match message.media()? {
        Media::WebPage(webpage) => match &webpage.raw.webpage {
            tl::enums::WebPage::Page(page) => page
                .photo
                .clone()
                .map(Photo::from_raw)
                .map(Media::Photo)
                .or(Some(Media::WebPage(webpage))),
            _ => Some(Media::WebPage(webpage)),
        },
        media => Some(media),
    }
}

async fn download_if_needed(client: &Client, media: &Media, path: &Path) -> Result<()> {
    let cached = path.metadata().is_ok_and(|metadata| {
        metadata.len() > 0
            && media
                .size()
                .is_none_or(|size| metadata.len() == size as u64)
    });
    if cached {
        return Ok(());
    }
    download_atomic(client, media, path).await
}

async fn download_atomic<D: Downloadable>(client: &Client, media: &D, path: &Path) -> Result<()> {
    let pending = PendingDownload(unique_temporary_path(path, "download"));
    // grammers::download_media drops its Tokio file without flushing. Small
    // downloads can still have a pending write when it returns, so checking
    // their size or starting ffmpeg immediately would reject valid images.
    let mut file = tokio::fs::File::create(&pending.0).await?;
    let mut download = client.iter_download(media);
    while let Some(chunk) = download
        .next()
        .await
        .with_context(|| format!("cannot download {}", path.display()))?
    {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    let length = fs::metadata(&pending.0)?.len();
    anyhow::ensure!(
        length > 0 && media.size().is_none_or(|size| length == size as u64),
        "incomplete media download for {}",
        path.display()
    );
    fs::rename(&pending.0, path)?;
    Ok(())
}

fn describe(media: &Media) -> (&'static str, Kind) {
    match media {
        Media::Photo(_) => (".jpg", Kind::Image),
        Media::Sticker(sticker) => {
            let mime = sticker.document.mime_type().unwrap_or("");
            if mime == "image/webp" {
                (".webp", Kind::Image)
            } else if mime.contains("webm") || mime.starts_with("video/") {
                (".webm", Kind::Animation)
            } else {
                (".tgs", Kind::Animation)
            }
        }
        Media::Document(document) => {
            let mime = document.mime_type().unwrap_or("application/octet-stream");
            let attributes = match &document.raw.document {
                Some(tl::enums::Document::Document(raw)) => raw.attributes.as_slice(),
                _ => &[],
            };
            let kind = classify_document_attributes(mime, attributes);
            let ext = match (extension_for(mime), kind) {
                (".bin", Kind::Video | Kind::Animation) => ".mp4",
                (".bin", Kind::Audio) => ".ogg",
                (extension, _) => extension,
            };
            (ext, kind)
        }
        _ => (".bin", Kind::File),
    }
}

fn classify(media: &Media) -> Kind {
    describe(media).1
}

fn classify_document(mime: &str) -> Kind {
    if mime == "application/x-tgsticker" {
        Kind::Animation
    } else if mime.starts_with("image/") {
        Kind::Image
    } else if mime.starts_with("video/") {
        Kind::Video
    } else if mime.starts_with("audio/") {
        Kind::Audio
    } else {
        Kind::File
    }
}

fn classify_document_attributes(mime: &str, attributes: &[tl::enums::DocumentAttribute]) -> Kind {
    // Telegram document attributes are authoritative even when MIME is generic
    // or stale. Animated MP4 files remain animations, not ordinary attachments.
    if attributes
        .iter()
        .any(|a| matches!(a, tl::enums::DocumentAttribute::Animated))
    {
        Kind::Animation
    } else if attributes
        .iter()
        .any(|a| matches!(a, tl::enums::DocumentAttribute::Video(_)))
    {
        Kind::Video
    } else if attributes
        .iter()
        .any(|a| matches!(a, tl::enums::DocumentAttribute::Audio(_)))
    {
        Kind::Audio
    } else {
        classify_document(mime)
    }
}

fn extension_for(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "audio/ogg" => ".ogg",
        "audio/mpeg" => ".mp3",
        "audio/mp4" => ".m4a",
        "application/x-tgsticker" => ".tgs",
        _ => ".bin",
    }
}

fn ensure_png(path: &Path) -> Result<PathBuf> {
    if path.extension().is_some_and(|extension| extension == "png") {
        complete_png_dimensions(path)?;
        return Ok(path.to_owned());
    }
    let png = path.with_extension("preview.png");
    if complete_png_dimensions(&png).is_ok() {
        return Ok(png);
    }
    let temporary = PendingDownload(unique_temporary_path(&png, "convert"));
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(path)
        .args(["-frames:v", "1"])
        .arg(&temporary.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("ffmpeg is required to convert this image")?;
    if !status.success() {
        bail!("ffmpeg could not convert {}", path.display());
    }
    complete_png_dimensions(&temporary.0)?;
    fs::rename(&temporary.0, &png)?;
    Ok(png)
}

fn complete_png_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut file = File::open(path)?;
    let mut header = [0; 24];
    file.read_exact(&mut header)?;
    let dimensions = png_dimensions(&header)?;
    file.seek(SeekFrom::End(-12))?;
    let mut end = [0; 12];
    file.read_exact(&mut end)?;
    anyhow::ensure!(end == *b"\0\0\0\0IEND\xaeB`\x82", "incomplete PNG image");
    Ok(dimensions)
}

fn ensure_tgs_frames(path: &Path) -> Result<Vec<PathBuf>> {
    decode_sticker_frames(path, ".tgs", Kind::Animation, |_| {})?
        .into_iter()
        .enumerate()
        .map(|(index, image)| {
            let png = PathBuf::from(format!(
                "{}-preview-{index:02}.png",
                path.with_extension("").display()
            ));
            fs::write(&png, image.png())?;
            Ok(png)
        })
        .collect()
}

fn tgs_command(path: &Path) -> Command {
    let mut command = Command::new("python3");
    command
        .args(["-c", include_str!("render_sticker.py")])
        .arg(path);
    command
}

fn ensure_tgs_preview(path: &Path) -> Result<PathBuf> {
    let stem = path.with_extension("");
    let png = PathBuf::from(format!("{}-preview-00.png", stem.display()));
    if png.exists() {
        return Ok(png);
    }
    let output = tgs_command(path)
        .arg("--single")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(crate::child_stderr())
        .output()
        .context("python-lottie and rsvg-convert are required for TGS")?;
    if !output.status.success() {
        bail!("cannot decode Telegram sticker {}", path.display());
    }
    MemoryImage::from_png(output.stdout.clone())?;
    fs::write(&png, output.stdout)?;
    Ok(png)
}

struct DisplayedImage {
    image: TerminalImage,
    row: u32,
    column: u32,
    rows: u32,
    columns: u32,
}

impl DisplayedImage {
    fn delete(mut self, terminal: &mut Terminal) -> Result<()> {
        self.image.delete(&mut terminal.stdout)?;
        terminal.refresh()?;
        Ok(())
    }
}

fn show_memory_overlay(image: &MemoryImage, terminal: &mut Terminal) -> Result<DisplayedImage> {
    let data = ImageData::from_png_bytes(image.png().to_vec())?;
    let mut terminal_image = TerminalImage::new(0x5447_0010, data)?;
    let (height, width) = terminal.getsize();
    let (pixel_width, pixel_height) = image.dimensions();
    let (columns, rows) = fit_dimensions(
        pixel_width,
        pixel_height,
        u32::from(width),
        u32::from(height).saturating_sub(2).max(1),
    );
    let row = u32::from(height).saturating_sub(2).saturating_sub(rows) / 2;
    let column = u32::from(width).saturating_sub(columns) / 2;
    terminal.setpos(row as u16, column as u16);
    terminal_image.upload(&mut terminal.stdout)?;
    place_overlay(&terminal_image, &mut terminal.stdout, columns, rows)?;
    terminal.refresh()?;
    Ok(DisplayedImage {
        image: terminal_image,
        row,
        column,
        rows,
        columns,
    })
}

const STORY_ACTIONS: &str = "[R] React    [M] Reply";

fn story_action_hit(terminal: &mut Terminal, x: u32, y: u32, video: bool) -> Option<ViewerAction> {
    let (height, width) = terminal.getsize();
    let row = u32::from(height.saturating_sub(if video { 2 } else { 1 }));
    let start = u32::from(width).saturating_sub(STORY_ACTIONS.len() as u32) / 2;
    if y != row || x < start || x >= start + STORY_ACTIONS.len() as u32 {
        return None;
    }
    Some(if x - start < 13 {
        ViewerAction::React
    } else {
        ViewerAction::Reply
    })
}

fn draw_story_action_bar(terminal: &mut Terminal, video: bool) -> Result<()> {
    let (height, width) = terminal.getsize();
    let row = height.saturating_sub(if video { 2 } else { 1 });
    let start = usize::from(width).saturating_sub(STORY_ACTIONS.len()) / 2;
    terminal.setpos(row, start as u16);
    write!(terminal.stdout, "\x1b[0m{STORY_ACTIONS}")?;
    terminal.refresh()?;
    Ok(())
}

fn choose_story_reaction(terminal: &mut Terminal) -> Result<Option<tl::enums::Reaction>> {
    const REACTIONS: [&str; 8] = ["👍", "❤️", "😂", "🔥", "👏", "😮", "😢", "🥰"];
    let (height, width) = terminal.getsize();
    let row = height.saturating_sub(1);
    let start = usize::from(width).saturating_sub(REACTIONS.len() * 5) / 2;
    terminal.setpos(row, 0);
    write!(terminal.stdout, "\x1b[0m\x1b[2K")?;
    for (index, reaction) in REACTIONS.iter().enumerate() {
        terminal.setpos(row, (start + index * 5) as u16);
        write!(terminal.stdout, "{}:{reaction}", index + 1)?;
    }
    terminal.setpos(row, (start + REACTIONS.len() * 5) as u16);
    write!(terminal.stdout, "0:clear")?;
    terminal.refresh()?;
    loop {
        let key = terminal.get_key(u64::MAX, false)?;
        match key {
            27 => return Ok(None),
            48 => return Ok(Some(tl::enums::Reaction::Empty)),
            49..=56 => {
                return Ok(Some(
                    tl::types::ReactionEmoji {
                        emoticon: REACTIONS[(key - 49) as usize].into(),
                    }
                    .into(),
                ));
            }
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::LEFT {
                    let x = usize::from(mouse.x);
                    if mouse.y == row && x >= start && x < start + REACTIONS.len() * 5 {
                        return Ok(Some(
                            tl::types::ReactionEmoji {
                                emoticon: REACTIONS[(x - start) / 5].into(),
                            }
                            .into(),
                        ));
                    }
                    if mouse.y == row
                        && x >= start + REACTIONS.len() * 5
                        && x < start + REACTIONS.len() * 5 + 7
                    {
                        return Ok(Some(tl::enums::Reaction::Empty));
                    }
                    return Ok(None);
                }
            }
            _ => {}
        }
    }
}

fn prompt_story_reply(terminal: &mut Terminal) -> Result<Option<String>> {
    let mut input = String::new();
    loop {
        let (height, width) = terminal.getsize();
        terminal.setpos(height.saturating_sub(1), 0);
        write!(terminal.stdout, "\x1b[0m\x1b[2KReply: ")?;
        let available = usize::from(width).saturating_sub(8);
        let visible: String = input
            .chars()
            .rev()
            .take(available)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        write!(terminal.stdout, "{visible}█")?;
        terminal.refresh()?;
        let key = terminal.get_key(u64::MAX, false)?;
        match key {
            27 => return Ok(None),
            10 | 13 => return Ok((!input.trim().is_empty()).then_some(input)),
            8 | 127 => {
                if let Some((index, _)) = input.grapheme_indices(true).next_back() {
                    input.truncate(index);
                }
            }
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::LEFT && mouse.y != height.saturating_sub(1) {
                    return Ok(None);
                }
            }
            _ => {
                let count = *terminal.readed.lock().unwrap();
                let buffer = terminal.reading_buffer.lock().unwrap();
                let raw = &buffer[..count];
                let text = terminalib::terminal::associated_key_text(raw)
                    .or_else(|| std::str::from_utf8(raw).ok().map(str::to_owned));
                if let Some(text) = text
                    && !text.chars().any(char::is_control)
                    && input.len() + text.len() <= 4096
                {
                    input.push_str(&text);
                }
            }
        }
    }
}

async fn send_story_reply(
    client: &Client,
    peer: PeerRef,
    story_id: i32,
    message: String,
) -> Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos() as u64;
    let random_id = (nanos
        ^ (u64::from(std::process::id()) << 32)
        ^ COUNTER.fetch_add(1, Ordering::Relaxed)) as i64;
    let reference: tl::enums::InputPeer = peer.into();
    client
        .invoke(&tl::functions::messages::SendMessage {
            no_webpage: false,
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: reference.clone(),
            reply_to: Some(
                tl::types::InputReplyToStory {
                    peer: reference,
                    story_id,
                }
                .into(),
            ),
            message,
            random_id,
            reply_markup: None,
            entities: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
            rich_message: None,
        })
        .await?;
    Ok(())
}

async fn show_progressive_image(
    client: Client,
    media: Media,
    path: PathBuf,
    preview_inputs: (Option<MemoryImage>, Option<Message>),
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    story_mode: bool,
) -> Result<ViewerAction> {
    let (preview, preview_source) = preview_inputs;
    let mut displayed = preview
        .as_ref()
        .map(|preview| show_memory_overlay(preview, terminal))
        .transpose()?;
    if story_mode {
        draw_story_action_bar(terminal, false)?;
    }

    let mut preview_loader = if displayed.is_none() {
        let preview_client = client.clone();
        let preview_media = media.clone();
        let preview_cache = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let preview_path = path.with_extension("thumbnail.jpg");
        Some(tokio::spawn(async move {
            if let Some(message) = preview_source {
                prepare_inline_preview(preview_client, message, preview_cache).await
            } else {
                prepare_media_thumbnail(preview_client, preview_media, preview_path).await
            }
        }))
    } else {
        None
    };
    let worker_path = path.clone();
    let loader = tokio::spawn(async move {
        download_if_needed(&client, &media, &path).await?;
        tokio::task::spawn_blocking(move || ensure_png(&worker_path))
            .await
            .context("image converter stopped")?
    });

    loop {
        if loader.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
        if preview_loader
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(task) = preview_loader.take()
            && let Ok(Ok(preview)) = task.await
        {
            displayed = Some(show_memory_overlay(&preview, terminal)?);
        }
        let bounds = displayed
            .as_ref()
            .map(|image| (image.row, image.column, image.rows, image.columns))
            .unwrap_or((0, 0, 0, 0));
        if let Some(action) = poll_viewer_action(terminal, bounds, 20, story_mode)? {
            loader.abort();
            let _ = loader.await;
            if let Some(task) = preview_loader.take() {
                task.abort();
                let _ = task.await;
            }
            if let Some(image) = displayed.take() {
                image.delete(terminal)?;
            }
            windows.invalidate();
            return Ok(action);
        }
    }

    let png = loader.await.context("image loader stopped")??;
    if let Some(task) = preview_loader.take() {
        task.abort();
        let _ = task.await;
    }
    if let Some(image) = displayed.take() {
        image.delete(terminal)?;
    }
    let action = show_tgp(&png, terminal, windows, story_mode)?;
    windows.invalidate();
    Ok(action)
}

fn poll_viewer_action(
    terminal: &mut Terminal,
    bounds: (u32, u32, u32, u32),
    timeout_ms: u64,
    story_mode: bool,
) -> Result<Option<ViewerAction>> {
    let key = terminal.get_key(timeout_ms, false)?;
    let (row, column, rows, columns) = bounds;
    Ok(match key {
        27 => Some(ViewerAction::Close),
        value if story_mode && value == i32::from(b'r') => Some(ViewerAction::React),
        value if story_mode && value == i32::from(b'm') => Some(ViewerAction::Reply),
        KEY_LEFT => Some(ViewerAction::Previous),
        KEY_RIGHT => Some(ViewerAction::Next),
        MOUSE_EVENT => {
            let mouse = terminal.getmouse();
            let x = u32::from(mouse.x);
            let y = u32::from(mouse.y);
            let inside = rows > 0
                && columns > 0
                && x >= column
                && x < column.saturating_add(columns)
                && y >= row
                && y < row.saturating_add(rows);
            if mouse.event == MouseEvent::LEFT
                && story_mode
                && let Some(action) = story_action_hit(terminal, x, y, false)
            {
                return Ok(Some(action));
            }
            (mouse.event == MouseEvent::LEFT && !inside).then_some(ViewerAction::Close)
        }
        _ => None,
    })
}

fn show_tgp(
    path: &Path,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    story_mode: bool,
) -> Result<ViewerAction> {
    let data = ImageData::from_png_file(path)?;
    let mut image = TerminalImage::new(0x5447_0001, data)?;
    let (height, width) = terminal.getsize();
    let usable_height = if story_mode {
        height.saturating_sub(2)
    } else {
        height
    };
    let (columns, rows) = fit_cells(path, u32::from(width), u32::from(usable_height))?;
    let row = u32::from(usable_height).saturating_sub(rows) / 2;
    let column = u32::from(width).saturating_sub(columns) / 2;
    terminal.setpos(row as u16, column as u16);
    image.upload(&mut terminal.stdout)?;
    place_overlay(&image, &mut terminal.stdout, columns, rows)?;
    if story_mode {
        draw_story_action_bar(terminal, false)?;
    }
    terminal.refresh()?;
    let action = wait_for_viewer_action(terminal, row, column, rows, columns, story_mode)?;
    image.delete(&mut terminal.stdout)?;
    terminal.refresh()?;
    windows.invalidate();
    Ok(action)
}

fn show_tgp_animation(
    paths: &[PathBuf],
    terminal: &mut Terminal,
    windows: &mut WindowManager,
) -> Result<ViewerAction> {
    let first = paths.first().context("sticker has no renderable frames")?;
    let data = ImageData::from_png_file(first)?;
    let mut image = TerminalImage::new(0x5447_0002, data)?;
    let (height, width) = terminal.getsize();
    let (columns, rows) = fit_cells(first, u32::from(width), u32::from(height))?;
    let row = u32::from(height).saturating_sub(rows) / 2;
    let column = u32::from(width).saturating_sub(columns) / 2;
    terminal.setpos(row as u16, column as u16);
    image.upload(&mut terminal.stdout)?;
    place_overlay(&image, &mut terminal.stdout, columns, rows)?;
    for path in paths.iter().skip(1) {
        image.push_frame(
            &mut terminal.stdout,
            ImageData::from_png_file(path)?,
            std::time::Duration::from_millis(83),
        )?;
    }
    image.start_animation(&mut terminal.stdout, None)?;
    terminal.refresh()?;
    let action = wait_for_viewer_action(terminal, row, column, rows, columns, false)?;
    image.stop_animation(&mut terminal.stdout)?;
    image.delete(&mut terminal.stdout)?;
    terminal.refresh()?;
    windows.invalidate();
    Ok(action)
}

fn place_overlay(
    image: &TerminalImage,
    writer: &mut impl Write,
    columns: u32,
    rows: u32,
) -> Result<()> {
    image.place_at_z(writer, columns, rows, OVERLAY_Z_INDEX)?;
    Ok(())
}

fn wait_for_viewer_action(
    terminal: &mut Terminal,
    row: u32,
    column: u32,
    rows: u32,
    columns: u32,
    story_mode: bool,
) -> Result<ViewerAction> {
    loop {
        match terminal.get_key(u64::MAX, false)? {
            27 => return Ok(ViewerAction::Close),
            value if story_mode && value == i32::from(b'r') => return Ok(ViewerAction::React),
            value if story_mode && value == i32::from(b'm') => return Ok(ViewerAction::Reply),
            KEY_LEFT => return Ok(ViewerAction::Previous),
            KEY_RIGHT => return Ok(ViewerAction::Next),
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::LEFT {
                    let x = u32::from(mouse.x);
                    let y = u32::from(mouse.y);
                    if story_mode && let Some(action) = story_action_hit(terminal, x, y, false) {
                        return Ok(action);
                    }
                    let inside = x >= column
                        && x < column.saturating_add(columns)
                        && y >= row
                        && y < row.saturating_add(rows);
                    if !inside {
                        return Ok(ViewerAction::Close);
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn fit_cells(path: &Path, max_columns: u32, max_rows: u32) -> Result<(u32, u32)> {
    let (pixel_width, pixel_height) = image_dimensions(path)?;
    Ok(fit_dimensions(
        pixel_width,
        pixel_height,
        max_columns,
        max_rows,
    ))
}

pub fn fit_dimensions(
    pixel_width: u32,
    pixel_height: u32,
    max_columns: u32,
    max_rows: u32,
) -> (u32, u32) {
    let source_ratio = pixel_width as f64 / pixel_height.max(1) as f64;
    let cell_aspect = terminal_cell_aspect();
    let max_columns = max_columns.max(1);
    let max_rows = max_rows.max(1);
    let mut best = (1, 1);
    let mut best_error = f64::INFINITY;
    let mut best_area = 0;
    for rows in 1..=max_rows {
        for columns in 1..=max_columns {
            let displayed_ratio = columns as f64 * cell_aspect / rows as f64;
            let error = (displayed_ratio / source_ratio).ln().abs();
            let area = columns * rows;
            if error + 1e-9 < best_error || ((error - best_error).abs() <= 1e-9 && area > best_area)
            {
                best = (columns, rows);
                best_error = error;
                best_area = area;
            }
        }
    }
    best
}

pub fn square_cells(max_columns: u32, max_rows: u32) -> (u32, u32) {
    fit_dimensions(1, 1, max_columns, max_rows)
}

pub fn image_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut header = [0u8; 24];
    File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .read_exact(&mut header)
        .with_context(|| format!("cannot read PNG header from {}", path.display()))?;
    png_dimensions(&header).with_context(|| format!("{} is not a PNG image", path.display()))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        bail!("missing PNG header");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        bail!("invalid PNG dimensions");
    }
    Ok((width, height))
}

pub fn terminal_cell_aspect() -> f64 {
    if let Ok(value) = std::env::var("TELEGA_CELL_ASPECT")
        && let Ok(value) = value.parse::<f64>()
        && (0.1..=2.0).contains(&value)
    {
        return value;
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 && size.ws_row > 0 && size.ws_xpixel > 0 && size.ws_ypixel > 0
    {
        let cell_width = f64::from(size.ws_xpixel) / f64::from(size.ws_col);
        let cell_height = f64::from(size.ws_ypixel) / f64::from(size.ws_row);
        if cell_height > 0.0 {
            return cell_width / cell_height;
        }
    }
    0.5
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
pub struct InlinePlayback {
    pipeline: VideoPipeline,
    source: crate::video_source::VideoSource,
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

impl InlinePlayback {
    pub fn new(client: Client, message: &Message, looping: bool, audible: bool) -> Result<Self> {
        let media = effective_media(message).context("inline video is unavailable")?;
        let duration = media_duration(&media).unwrap_or_default();
        let source = crate::video_source::VideoSource::with_cache(
            client,
            media,
            if looping { 4 } else { 24 },
        )?;
        let options = DecoderOptions {
            round: is_circle(message),
            looping,
            compatible: false,
        };
        let mut pipeline = spawn_video_pipeline_with(&source.url, 0.0, 10, 256, options)?;
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
                let (width, height) = png_dimensions(&png)?;
                self.frame = Some(MemoryImage {
                    png: Arc::new(png),
                    width,
                    height,
                });
                self.frames += 1;
                self.position = self.offset + self.frames as f64 / 10.0;
                self.serial = self.serial.wrapping_add(1);
                if !self.pipeline.audio_started {
                    self.pipeline.audio = Some(spawn_audio_input(&self.source.url, self.offset)?);
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
                        &self.source.url,
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
                            _ => "inline decoder produced no frames; see telega.log".to_owned(),
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
            spawn_video_pipeline_with(&self.source.url, self.offset, 10, 256, self.options)?;
        self.pipeline.audio_started = !audible;
        self.position = self.offset;
        self.frames = 0;
        self.ended = false;
        self.paused = false;
        Ok(())
    }
}

pub fn viewer_backdrop(terminal: &mut Terminal) -> Result<TerminalImage> {
    let mut image = TerminalImage::new(
        0x5447_0030,
        ImageData::solid(1, 1, terminalib::Rgba::new(0, 0, 0, 180))?,
    )?;
    let (height, width) = terminal.getsize();
    terminal.setpos(0, 0);
    image.upload(&mut terminal.stdout)?;
    image.place_at_z(
        &mut terminal.stdout,
        u32::from(width),
        u32::from(height),
        i32::MAX - 2,
    )?;
    terminal.refresh()?;
    Ok(image)
}

pub fn clear_viewer_layers(terminal: &mut Terminal) -> Result<()> {
    let mut group = ImageGroup::begin(&mut terminal.stdout)?;
    for id in [
        0x5447_0001u32,
        0x5447_0002,
        0x5447_0003,
        0x5447_0004,
        0x5447_0010,
        0x5447_0020,
        0x5447_0021,
        0x5447_0030,
    ] {
        write!(group, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
    }
    group.present()?;
    Ok(())
}

fn spawn_video_pipeline(
    input: &str,
    offset: f64,
    fps: u32,
    decode_width: u32,
) -> Result<VideoPipeline> {
    spawn_video_pipeline_with(input, offset, fps, decode_width, DecoderOptions::default())
}

#[derive(Clone, Copy, Default)]
struct DecoderOptions {
    compatible: bool,
    round: bool,
    looping: bool,
}

fn spawn_video_pipeline_with(
    input: &str,
    offset: f64,
    fps: u32,
    decode_width: u32,
    options: DecoderOptions,
) -> Result<VideoPipeline> {
    let mut command = Command::new("ffmpeg");
    command.args(["-loglevel", "warning", "-nostdin"]);
    if input.starts_with("http://") {
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
        .stderr(crate::child_stderr())
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

async fn play_video_stream_tgp(
    client: Client,
    media: Media,
    preview: Option<MemoryImage>,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    advance_on_end: bool,
    story_mode: bool,
) -> Result<ViewerAction> {
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
    let duration = media_duration(&media).unwrap_or_default();
    let source = crate::video_source::VideoSource::new(client, media)?;
    let mut displayed = preview
        .as_ref()
        .map(|image| show_memory_overlay(image, terminal))
        .transpose()?;
    let mut pipeline = spawn_video_pipeline(&source.url, 0.0, fps, decode_width)?;
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
                        &source.url,
                        0.0,
                        fps,
                        decode_width,
                        DecoderOptions {
                            compatible,
                            ..Default::default()
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
                    &source.url,
                    segment_start,
                    fps,
                    decode_width,
                    DecoderOptions {
                        compatible,
                        ..Default::default()
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
                        pipeline.audio = spawn_audio_input(&source.url, segment_start).ok();
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
                            &source.url,
                            segment_start,
                            fps,
                            decode_width,
                            DecoderOptions {
                                compatible: true,
                                ..Default::default()
                            },
                        )?;
                    } else {
                        if premature {
                            failure = Some(match result {
                                Ok(Err(error)) => error,
                                _ => "ffmpeg ended before the video finished; see telega.log"
                                    .to_owned(),
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
        let key = (
            shown as u64,
            (shown / duration.max(1.0) * 36.0) as u32,
            source.fetched_bytes() * 100 / source.size(),
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
                    downloaded: source.fetched_bytes().min(source.size()),
                    total: source.size(),
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

fn media_duration(media: &Media) -> Option<f64> {
    match media {
        Media::Document(document) => document.duration(),
        Media::Sticker(sticker) => sticker.document.duration(),
        _ => None,
    }
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

fn read_png_frame(reader: &mut impl Read) -> Result<Option<Vec<u8>>> {
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
    spawn_audio_input(path.to_str().context("audio path is not UTF-8")?, offset)
}

fn spawn_audio_input(input: &str, offset: f64) -> Result<Child> {
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

fn spawn_audio(path: &Path) -> Result<Child> {
    spawn_audio_at(path, 0.0)
}

fn play_audio(path: &Path, terminal: &mut Terminal) -> Result<()> {
    let mut child = spawn_audio(path)?;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("audio player exited with status {status}");
        }
        if terminal.get_key(25, false)? == 27 {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canceling_preview_keeps_converter_input_until_worker_finishes() {
        let original = std::env::temp_dir().join("telega-worker-lifetime.jpg");
        let temporary = TemporaryMedia::new(&original);
        let path = temporary.path().to_owned();
        let derivative = path.with_extension("preview.png");
        fs::write(&path, b"source").unwrap();
        fs::write(&derivative, b"preview").unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = mpsc::channel();
        let (finished, done) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = released.recv();
                drop(temporary);
                let _ = finished.send(());
            })
            .await
        });
        ready.await.unwrap();
        task.abort();
        let _ = task.await;
        let converter_input_exists = path.exists();
        release.send(()).unwrap();
        done.await.unwrap();
        assert!(converter_input_exists);
        assert!(!path.exists());
        assert!(!derivative.exists());
    }

    #[test]
    fn interrupted_download_does_not_publish_a_partial_file() {
        let target =
            unique_temporary_path(&std::env::temp_dir().join("telega-complete.jpg"), "test");
        fs::write(&target, b"complete").unwrap();
        let pending = PendingDownload(unique_temporary_path(&target, "download"));
        let partial = pending.0.clone();
        fs::write(&partial, b"partial").unwrap();
        drop(pending);
        assert!(!partial.exists());
        assert_eq!(fs::read(&target).unwrap(), b"complete");
        fs::remove_file(target).unwrap();
    }

    #[tokio::test]
    async fn downloaded_media_is_flushed_before_validation_and_publication() {
        struct Embedded(Vec<u8>);
        impl Downloadable for Embedded {
            fn to_raw_input_location(&self) -> Option<tl::enums::InputFileLocation> {
                None
            }
            fn to_data(&self) -> Option<Vec<u8>> {
                Some(self.0.clone())
            }
            fn size(&self) -> Option<usize> {
                Some(self.0.len())
            }
        }
        let pool = grammers_mtsender::SenderPool::new(
            Arc::new(grammers_session::storages::MemorySession::default()),
            1,
        );
        let client = Client::new(pool.handle.clone());
        for size in [1_000, 4_096, 65_536, 524_288] {
            for _ in 0..8 {
                let target = PendingDownload(unique_temporary_path(
                    &std::env::temp_dir().join("telega-flush-test.jpg"),
                    "test",
                ));
                let media = Embedded(vec![42; size]);
                download_atomic(&client, &media, &target.0).await.unwrap();
                assert_eq!(fs::read(&target.0).unwrap(), media.0);
            }
        }
    }

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
    fn cached_png_requires_iend_at_the_end_of_the_file() {
        let image = include_bytes!("../vendor/terminalib/tests/imgs/err.png");
        let path =
            unique_temporary_path(&std::env::temp_dir().join("telega-png-cache.png"), "test");
        let temporary = PendingDownload(path.clone());
        fs::write(&path, &image[..24]).unwrap();
        assert!(complete_png_dimensions(&path).is_err());
        fs::write(&path, image).unwrap();
        assert!(complete_png_dimensions(&path).is_ok());
        let mut trailing = image.to_vec();
        trailing.extend_from_slice(b"incomplete-next-chunk");
        fs::write(&path, trailing).unwrap();
        assert!(complete_png_dimensions(&path).is_err());
        drop(temporary);
    }

    #[test]
    fn file_labels_show_the_basename_without_control_characters() {
        assert_eq!(
            file_label(Some("/tmp/отчёт final.pdf")),
            "📎 отчёт final.pdf"
        );
        assert_eq!(file_label(Some("C:\\files\\notes.txt")), "📎 notes.txt");
        assert_eq!(file_label(Some("bad\n\u{202e}name.txt")), "📎 badname.txt");
        assert_eq!(file_label(None), "📎 file");
    }

    #[test]
    fn chat_avatar_is_round_without_changing_the_original() {
        if Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return;
        }
        let directory = std::env::temp_dir().join(format!(
            "telega-avatar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("original.png");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=120x60",
                "-frames:v",
                "1",
                "-y",
            ])
            .arg(&source)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let round = ensure_round_avatar(&source).unwrap();
        assert_eq!(image_dimensions(&source).unwrap(), (120, 60));
        assert_eq!(image_dimensions(&round).unwrap(), (60, 60));
        assert_eq!(round, ensure_round_avatar(&source).unwrap());
        let pixels = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&round)
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1"])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(pixels.status.success());
        assert_eq!(pixels.stdout.len(), 60 * 60 * 4);
        assert_eq!(pixels.stdout[3], 0);
        assert_eq!(pixels.stdout[(30 * 60 + 30) * 4 + 3], 255);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn large_webm_stickers_keep_alpha_and_fit_a_bounded_frame() {
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
        let dir = std::env::temp_dir().join(format!("telega-webm-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("large.webm");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red@0.5:size=512x1024:rate=10:duration=0.3,format=yuva420p",
                    "-c:v",
                    "libvpx-vp9",
                    "-lossless",
                    "1",
                    "-auto-alt-ref",
                    "0",
                ])
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        let frames = decode_sticker_frames(&path, ".webm", Kind::Animation, |_| {}).unwrap();
        assert!(frames.len() >= 2);
        assert_eq!(frames[0].dimensions(), (128, 256));
        let first = dir.join("first.png");
        fs::write(&first, frames[0].png()).unwrap();
        let pixels = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&first)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "pipe:1",
            ])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(pixels.status.success());
        assert!(
            pixels
                .stdout
                .as_chunks::<4>()
                .0
                .iter()
                .any(|p| p[3] > 0 && p[3] < 255),
            "WEBM alpha must survive decoding"
        );
        fs::remove_file(first).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn large_tgs_stickers_use_their_timeline_and_stream_a_visible_first_frame() {
        use flate2::{Compression, write::GzEncoder};
        if !Command::new("python3")
            .args(["-c", "import lottie"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return;
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("telega-sticker-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("large.tgs");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(include_bytes!("fixtures/sticker-offset.json"))
            .unwrap();
        fs::write(&path, encoder.finish().unwrap()).unwrap();
        let mut received_first = false;
        let frames = decode_sticker_frames(&path, ".tgs", Kind::Animation, |image| {
            assert_eq!(image.dimensions(), (256, 256));
            received_first = true;
        })
        .unwrap();
        assert!(received_first);
        assert_eq!(frames.len(), 10);
        assert_ne!(frames.first().unwrap().png(), frames.last().unwrap().png());
        let first = dir.join("first.png");
        fs::write(&first, frames[0].png()).unwrap();
        let pixels = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&first)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "pipe:1",
            ])
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(pixels.status.success());
        assert!(
            pixels
                .stdout
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] > 0),
            "nonzero in_point must not render a transparent frame 0"
        );
        fs::remove_file(first).unwrap();
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn generic_mime_does_not_hide_telegram_video_voice_or_animation_attributes() {
        assert_eq!(
            classify_document_attributes("application/x-tgsticker", &[]),
            Kind::Animation
        );
        let video = tl::types::DocumentAttributeVideo {
            round_message: true,
            supports_streaming: true,
            nosound: false,
            duration: 3.0,
            w: 240,
            h: 240,
            preload_prefix_size: None,
            video_start_ts: None,
            video_codec: None,
        }
        .into();
        assert_eq!(
            classify_document_attributes("application/octet-stream", &[video]),
            Kind::Video
        );
        let audio = tl::types::DocumentAttributeAudio {
            voice: true,
            duration: 2,
            title: None,
            performer: None,
            waveform: None,
        }
        .into();
        assert_eq!(
            classify_document_attributes("application/octet-stream", &[audio]),
            Kind::Audio
        );
        assert_eq!(
            classify_document_attributes("video/mp4", &[tl::enums::DocumentAttribute::Animated]),
            Kind::Animation
        );
        assert_eq!(
            classify_document_attributes("application/pdf", &[]),
            Kind::File
        );
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
        let input = path.to_str().unwrap();
        let mut pipeline = spawn_video_pipeline(input, 0.0, 4, 64).unwrap();
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
        let mut sought = spawn_video_pipeline(input, 1.5, 4, 64).unwrap();
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
