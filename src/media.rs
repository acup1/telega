use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc;

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

use terminalib::{MOUSE_EVENT, MouseEvent};
use tokio::io::AsyncWriteExt;
use unicode_segmentation::UnicodeSegmentation;

use crate::telegram::Telegram;

pub use tgp_viewer::{
    MemoryImage, ViewerContext, clear_viewer_layers, fit_dimensions, image_dimensions,
    set_audio_paused, spawn_audio_at, square_cells, terminal_cell_aspect, viewer_backdrop,
};
use tgp_viewer::{
    PlaybackOptions, ViewerAction, draw_story_action_bar, png_dimensions, poll_viewer_action,
    read_png_frame, show_memory_overlay, show_tgp, show_tgp_animation, show_unsupported,
};
const GALLERY_PAGE: usize = 50;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Image,
    Video,
    Audio,
    Animation,
    File,
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
    MemoryImage::from_png(png)
        .with_context(|| format!("cannot read PNG dimensions from {}", path.display()))
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
            .map(tgp_viewer::DisplayedImage::bounds)
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

impl tgp_viewer::VideoInputSource for crate::video_source::VideoSource {
    fn input(&self) -> &std::ffi::OsStr {
        self.url.as_ref()
    }
    fn buffered(&self) -> (u64, u64) {
        (self.fetched_bytes(), self.size())
    }
}

pub type InlinePlayback = tgp_viewer::InlinePlayback<crate::video_source::VideoSource>;
pub fn inline_playback(
    client: Client,
    message: &Message,
    looping: bool,
    audible: bool,
) -> Result<InlinePlayback> {
    let media = effective_media(message).context("inline video is unavailable")?;
    let duration = media_duration(&media).unwrap_or_default();
    let source =
        crate::video_source::VideoSource::with_cache(client, media, if looping { 4 } else { 24 })?;
    InlinePlayback::new(
        source,
        duration,
        looping,
        audible,
        is_circle(message),
        crate::child_stderr,
    )
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
    let duration = media_duration(&media).unwrap_or_default();
    let source = crate::video_source::VideoSource::new(client, media)?;
    tgp_viewer::play_video(
        &source,
        duration,
        preview,
        terminal,
        windows,
        PlaybackOptions {
            advance_on_end,
            story_mode,
            stderr: crate::child_stderr,
        },
    )
    .await
}

fn media_duration(media: &Media) -> Option<f64> {
    match media {
        Media::Document(document) => document.duration(),
        Media::Sticker(sticker) => sticker.document.duration(),
        _ => None,
    }
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
    fn cached_png_requires_iend_at_the_end_of_the_file() {
        let image = include_bytes!("fixtures/terminalib-test.png");
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
}
