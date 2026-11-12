use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use grammers_client::message::InputMessage;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_client::{Client, SignInError};
use grammers_mtsender::{SenderPool, SenderPoolFatHandle};
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerId, PeerKind, PeerRef};

pub struct Telegram {
    pub client: Client,
    handle: SenderPoolFatHandle,
    updates: Option<tokio::sync::mpsc::Receiver<grammers_session::updates::UpdatesLike>>,
}

#[derive(Clone)]
pub struct Chat {
    pub name: String,
    pub peer: PeerRef,
    pub peer_object: Peer,
    pub last: String,
    pub last_message: Option<grammers_client::message::Message>,
    pub presence: String,
    pub archived: bool,
    pub unread: i32,
    pub read_inbox_max_id: i32,
    pub read_outbox_max_id: i32,
    pub folders: Vec<i32>,
    pub search_only: bool,
    kind: ChatKind,
    is_contact: bool,
    is_bot: bool,
}

#[derive(Clone)]
enum ChatKind {
    User,
    Group,
    Channel,
}

#[derive(Clone)]
pub struct Folder {
    pub id: i32,
    pub name: String,
    pub icon: Option<String>,
}

pub struct Discussion {
    pub peer: PeerRef,
    pub peer_object: Peer,
    pub name: String,
    pub root_id: i32,
    pub post_id: i32,
    pub messages: Vec<grammers_client::message::Message>,
    pub has_more: bool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum AttachmentKind {
    Media,
    File,
    Audio,
}

impl Chat {
    pub fn from_search_peer(peer_object: Peer, peer: PeerRef) -> Self {
        let (kind, is_contact, is_bot) = match &peer_object {
            Peer::User(user) => (ChatKind::User, user.contact(), user.is_bot()),
            Peer::Group(_) | Peer::Community(_) => (ChatKind::Group, false, false),
            Peer::Channel(_) => (ChatKind::Channel, false, false),
        };
        Self {
            name: peer_object.name().unwrap_or("Deleted account").to_owned(),
            peer,
            presence: presence(&peer_object),
            peer_object,
            last: String::new(),
            last_message: None,
            archived: false,
            unread: 0,
            read_inbox_max_id: 0,
            read_outbox_max_id: 0,
            folders: Vec::new(),
            search_only: true,
            kind,
            is_contact,
            is_bot,
        }
    }
}

impl Telegram {
    pub fn resolve_forward_source(
        &self,
        id: PeerId,
        context: PeerRef,
        message_id: i32,
    ) -> impl std::future::Future<Output = Result<Chat>> + Send + 'static {
        let client = self.client.clone();
        let session = self.handle.session.clone();
        async move {
            let cached = session
                .peer_ref(id)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            if let Some(reference) = cached
                && let Ok(peer) = client.resolve_peer(reference).await
            {
                return Ok(Chat::from_search_peer(peer, reference));
            }
            // Forward metadata may reference a min peer without an access
            // hash. Telegram can resolve it through the containing message.
            let bare_id = id.bare_id().context("forward source has no identifier")?;
            let peer = match id.kind() {
                PeerKind::User => {
                    let users = client
                        .invoke(&tl::functions::users::GetUsers {
                            id: vec![
                                tl::types::InputUserFromMessage {
                                    peer: context.into(),
                                    msg_id: message_id,
                                    user_id: bare_id,
                                }
                                .into(),
                            ],
                        })
                        .await?;
                    users
                        .into_iter()
                        .map(|raw| Peer::User(grammers_client::peer::User::from_raw(&client, raw)))
                        .find(|peer| peer.id() == id)
                        .context("forward sender is unavailable")?
                }
                PeerKind::Channel => {
                    let response = client
                        .invoke(&tl::functions::channels::GetChannels {
                            id: vec![
                                tl::types::InputChannelFromMessage {
                                    peer: context.into(),
                                    msg_id: message_id,
                                    channel_id: bare_id,
                                }
                                .into(),
                            ],
                        })
                        .await?;
                    let chats = match response {
                        tl::enums::messages::Chats::Chats(chats) => chats.chats,
                        tl::enums::messages::Chats::Slice(chats) => chats.chats,
                    };
                    chats
                        .into_iter()
                        .map(|raw| Peer::from_raw(&client, raw))
                        .find(|peer| peer.id() == id)
                        .context("forward channel is unavailable")?
                }
                PeerKind::Chat => {
                    client
                        .resolve_peer(PeerRef {
                            id,
                            auth: Default::default(),
                        })
                        .await?
                }
            };
            let reference = peer
                .to_ref()
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?
                .context("forward source is inaccessible")?;
            Ok(Chat::from_search_peer(peer, reference))
        }
    }

    pub async fn connect(api_id: i32, session_path: &Path) -> Result<Self> {
        let session = Arc::new(
            SqliteSession::open(session_path)
                .await
                .with_context(|| format!("cannot open session {}", session_path.display()))?,
        );
        let SenderPool {
            runner,
            handle,
            updates,
        } = SenderPool::new(session, api_id);
        let client = Client::new(handle.clone());
        tokio::spawn(runner.run());
        Ok(Self {
            client,
            handle,
            updates: Some(updates),
        })
    }

    pub async fn take_updates(&mut self) -> Result<grammers_client::client::UpdateStream> {
        let receiver = self
            .updates
            .take()
            .context("updates stream already started")?;
        self.client
            .stream_updates(
                receiver,
                grammers_mtsender::UpdatesConfiguration { catch_up: true },
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    pub async fn authorize(&self, api_hash: &str) -> Result<()> {
        if self.client.is_authorized().await? {
            return Ok(());
        }
        println!("First launch: sign in to Telegram.");
        let phone = prompt("Phone number in international format: ")?;
        let token = self
            .client
            .request_login_code(phone.trim(), api_hash)
            .await
            .context("Telegram did not accept the phone number")?;
        let code = prompt("Code from Telegram: ")?;
        match self.client.sign_in(&token, code.trim()).await {
            Ok(_) => {}
            Err(SignInError::PasswordRequired(password_token)) => {
                let hint = password_token.hint().unwrap_or("no hint");
                let password = rpassword::prompt_password(format!("2FA password ({hint}): "))?;
                self.client
                    .check_password(password_token, password)
                    .await
                    .context("2FA password was rejected")?;
            }
            Err(SignInError::SignUpRequired) => {
                bail!("sign up with an official Telegram client first")
            }
            Err(error) => return Err(error.into()),
        }
        println!("Signed in. Session saved.");
        Ok(())
    }

    pub async fn chats(&self, limit: usize) -> Result<(Vec<Chat>, Vec<Folder>)> {
        Self::chats_with(&self.client, limit).await
    }

    pub async fn chats_with(client: &Client, limit: usize) -> Result<(Vec<Chat>, Vec<Folder>)> {
        let raw_filters: tl::types::messages::DialogFilters = client
            .invoke(&tl::functions::messages::GetDialogFilters {})
            .await?
            .into();
        let mut folders: Vec<Folder> = raw_filters
            .filters
            .iter()
            .map(|filter| match filter {
                tl::enums::DialogFilter::Default => Folder {
                    id: 0,
                    name: "All chats".to_owned(),
                    icon: None,
                },
                tl::enums::DialogFilter::Filter(filter) => Folder {
                    id: filter.id,
                    name: text_with_entities(&filter.title).to_owned(),
                    icon: filter.emoticon.clone(),
                },
                tl::enums::DialogFilter::Chatlist(filter) => Folder {
                    id: filter.id,
                    name: text_with_entities(&filter.title).to_owned(),
                    icon: filter.emoticon.clone(),
                },
            })
            .collect();
        if !folders.iter().any(|folder| folder.id == 0) {
            folders.insert(
                0,
                Folder {
                    id: 0,
                    name: "All chats".to_owned(),
                    icon: None,
                },
            );
        }
        folders.push(Folder {
            id: -1,
            name: "Archive".to_owned(),
            icon: None,
        });
        let shared: Vec<_> = raw_filters
            .filters
            .iter()
            .filter_map(|filter| match filter {
                tl::enums::DialogFilter::Chatlist(filter) => Some(filter.clone()),
                _ => None,
            })
            .collect();
        let filters: Vec<_> = raw_filters
            .filters
            .into_iter()
            .filter_map(|filter| match filter {
                tl::enums::DialogFilter::Filter(filter) => Some(filter),
                _ => None,
            })
            .collect();

        let mut result = Vec::new();
        let mut dialogs = client.iter_dialogs();
        while result.len() < limit {
            let Some(dialog) = dialogs.next().await? else {
                break;
            };
            let last = dialog
                .last_message
                .as_ref()
                .map(|message| message.text().replace('\n', " "))
                .unwrap_or_default();
            let (archived, unread) = match &dialog.raw {
                tl::enums::Dialog::Dialog(raw) => (raw.folder_id == Some(1), raw.unread_count),
                _ => (false, 0),
            };
            let peer_object = dialog.peer().clone();
            let (kind, is_contact, is_bot) = match &peer_object {
                Peer::User(user) => (ChatKind::User, user.contact(), user.is_bot()),
                Peer::Group(_) | Peer::Community(_) => (ChatKind::Group, false, false),
                Peer::Channel(_) => (ChatKind::Channel, false, false),
            };
            let mut chat = Chat {
                name: dialog.peer().name().unwrap_or("Deleted account").to_owned(),
                peer: dialog.peer_ref(),
                peer_object,
                last,
                last_message: dialog.last_message.clone(),
                presence: presence(dialog.peer()),
                archived,
                unread,
                read_inbox_max_id: match &dialog.raw {
                    tl::enums::Dialog::Dialog(raw) => raw.read_inbox_max_id,
                    _ => 0,
                },
                read_outbox_max_id: match &dialog.raw {
                    tl::enums::Dialog::Dialog(raw) => raw.read_outbox_max_id,
                    _ => 0,
                },
                folders: Vec::new(),
                search_only: false,
                kind,
                is_contact,
                is_bot,
            };
            chat.folders = filters
                .iter()
                .filter(|filter| matches_filter(&chat, filter))
                .map(|filter| filter.id)
                .collect();
            chat.folders.extend(
                shared
                    .iter()
                    .filter(|filter| {
                        filter
                            .include_peers
                            .iter()
                            .chain(&filter.pinned_peers)
                            .any(|peer| input_peer_matches(peer, chat.peer.id))
                    })
                    .map(|filter| filter.id),
            );
            result.push(chat);
        }
        Ok((result, folders))
    }

    pub async fn messages_with(
        client: &Client,
        peer: PeerRef,
        limit: usize,
    ) -> Result<Vec<grammers_client::message::Message>> {
        Self::messages_before(client, peer, limit, 0).await
    }

    pub async fn messages_before(
        client: &Client,
        peer: PeerRef,
        limit: usize,
        offset_id: i32,
    ) -> Result<Vec<grammers_client::message::Message>> {
        let mut messages = client.iter_messages(peer).offset_id(offset_id).limit(limit);
        let mut result = Vec::new();
        while let Some(message) = messages.next().await? {
            result.push(message);
        }
        result.reverse();
        Ok(result)
    }

    pub async fn messages_after(
        client: &Client,
        peer: PeerRef,
        limit: usize,
        offset_id: i32,
    ) -> Result<Vec<grammers_client::message::Message>> {
        let mut messages = client
            .iter_messages(peer)
            .offset_id(offset_id)
            .reverse(true)
            .limit(limit);
        let mut result = Vec::new();
        while let Some(message) = messages.next().await? {
            result.push(message);
        }
        Ok(result)
    }

    pub async fn discussion(
        client: &Client,
        channel: PeerRef,
        post_id: i32,
        limit: usize,
        offset_id: i32,
    ) -> Result<Discussion> {
        let raw = client
            .invoke(&tl::functions::messages::GetDiscussionMessage {
                peer: channel.into(),
                msg_id: post_id,
            })
            .await
            .context("this post has no accessible discussion")?;
        let tl::enums::messages::DiscussionMessage::Message(raw) = raw;

        let linked = raw
            .chats
            .into_iter()
            .map(|chat| Peer::from_raw(client, chat))
            .find(|peer| peer.id() != channel.id)
            .context("the channel has no linked discussion chat")?;
        let peer = linked
            .to_ref()
            .await
            .map_err(anyhow::Error::msg)?
            .context("the linked discussion chat is not accessible")?;
        let root_id = raw
            .messages
            .iter()
            .find(|message| raw_message_peer_id(message).is_some_and(|id| id == linked.id()))
            .map(tl::enums::Message::id)
            .context("Telegram did not return the discussion root message")?;
        let name = linked.name().unwrap_or("Discussion").to_owned();

        let replies = client
            .invoke(&tl::functions::messages::GetReplies {
                peer: channel.into(),
                msg_id: post_id,
                offset_id,
                offset_date: 0,
                add_offset: 0,
                limit: i32::try_from(limit).unwrap_or(i32::MAX),
                max_id: 0,
                min_id: 0,
                hash: 0,
            })
            .await?;
        let ids = raw_message_ids(replies)
            .into_iter()
            .filter(|id| *id != root_id)
            .collect::<Vec<_>>();
        let mut messages = client
            .get_messages_by_id(peer, &ids)
            .await?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        messages.sort_by_key(grammers_client::message::Message::id);
        let has_more = ids.len() == limit;

        Ok(Discussion {
            peer,
            peer_object: linked,
            name,
            root_id,
            post_id,
            messages,
            has_more,
        })
    }

    pub async fn linked_discussion(
        client: &Client,
        channel: PeerRef,
        post_id: i32,
        limit: usize,
    ) -> Result<Discussion> {
        let mut discussion = Self::discussion(client, channel, post_id, 1, 0).await?;
        discussion.messages = Self::messages_with(client, discussion.peer, limit).await?;
        discussion.has_more = discussion.messages.len() == limit;
        Ok(discussion)
    }

    pub async fn send(
        &self,
        peer: PeerRef,
        text: &str,
    ) -> Result<grammers_client::message::Message> {
        Ok(self
            .client
            .send_message(peer, crate::message_format::message(text, &[], false))
            .await?)
    }

    pub async fn send_attachment(
        &self,
        peer: PeerRef,
        reply_to: Option<i32>,
        path: &Path,
        kind: AttachmentKind,
    ) -> Result<grammers_client::message::Message> {
        Self::send_attachment_with(&self.client, peer, reply_to, path, kind).await
    }

    pub async fn send_attachment_with(
        client: &Client,
        peer: PeerRef,
        reply_to: Option<i32>,
        path: &Path,
        kind: AttachmentKind,
    ) -> Result<grammers_client::message::Message> {
        if !path.is_file() {
            bail!("attachment does not exist: {}", path.display());
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let photo = matches!(
            extension.as_str(),
            "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif"
        );
        let video = matches!(extension.as_str(), "mp4" | "mov" | "mkv" | "webm");
        let audio = matches!(
            extension.as_str(),
            "mp3" | "m4a" | "ogg" | "opus" | "flac" | "wav" | "aac"
        );
        if kind == AttachmentKind::Media && !photo && !video {
            bail!("choose a photo or video");
        }
        if kind == AttachmentKind::Audio && !audio {
            bail!("choose an audio file");
        }
        let uploaded = if kind == AttachmentKind::Media && photo {
            let original = path.to_path_buf();
            let bytes = tokio::task::spawn_blocking(move || compress_photo(&original))
                .await
                .context("photo compressor stopped")??;
            let name = format!(
                "{}.jpg",
                path.file_stem().unwrap_or_default().to_string_lossy()
            );
            client
                .upload_stream(&mut bytes.as_slice(), bytes.len(), name)
                .await?
        } else {
            client
                .upload_file(path)
                .await
                .with_context(|| format!("cannot upload {}", path.display()))?
        };
        let mut message = match kind {
            AttachmentKind::File => InputMessage::new().file(uploaded),
            AttachmentKind::Media if photo => InputMessage::new().photo(uploaded),
            _ => InputMessage::new().document(uploaded),
        };
        if kind != AttachmentKind::File
            && (video || audio)
            && let Ok(output) = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration:stream=width,height",
                    "-of",
                    "default=noprint_wrappers=1",
                ])
                .arg(path)
                .stderr(std::process::Stdio::null())
                .output()
            && output.status.success()
        {
            let metadata = String::from_utf8_lossy(&output.stdout);
            let number = |name: &str| {
                metadata
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix(name)
                            .and_then(|value| value.parse::<f64>().ok())
                    })
                    .unwrap_or_default()
            };
            let duration = std::time::Duration::from_secs_f64(number("duration=").max(0.0));
            if video {
                message = message.attribute(grammers_client::media::Attribute::Video {
                    round_message: false,
                    supports_streaming: true,
                    duration,
                    w: number("width=") as i32,
                    h: number("height=") as i32,
                });
            } else {
                message = message.attribute(grammers_client::media::Attribute::Audio {
                    duration,
                    title: path.file_stem().and_then(|s| s.to_str()).map(str::to_owned),
                    performer: None,
                });
            }
        }
        Ok(client
            .send_message(peer, message.reply_to(reply_to))
            .await?)
    }

    pub async fn send_reply(
        &self,
        peer: PeerRef,
        root_id: i32,
        text: &str,
    ) -> Result<grammers_client::message::Message> {
        Ok(self
            .client
            .send_message(
                peer,
                crate::message_format::message(text, &[], false).reply_to(Some(root_id)),
            )
            .await?)
    }

    pub fn peer_name(peer: Option<&Peer>) -> &str {
        peer.and_then(Peer::name).unwrap_or("Unknown")
    }
}

fn compress_photo(path: &Path) -> Result<Vec<u8>> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "scale=w='min(2560,iw)':h='min(2560,ih)':force_original_aspect_ratio=decrease,setsar=1",
            "-q:v",
            "3",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(crate::child_stderr())
        .output()
        .context("ffmpeg is required to compress photos")?;
    anyhow::ensure!(
        output.status.success() && !output.stdout.is_empty(),
        "cannot compress photo; try sending as a file"
    );
    Ok(output.stdout)
}

#[cfg(test)]
#[test]
fn photo_compression_preserves_aspect_and_never_changes_original_file() {
    use std::{
        fs,
        process::{Command, Stdio},
    };

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
    let dir = std::env::temp_dir().join(format!("telega-photo-{}-{unique}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let original = dir.join("original.png");
    assert!(
        Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=4000x1000",
                "-frames:v",
                "1"
            ])
            .arg(&original)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let unchanged = fs::read(&original).unwrap();
    let compressed = compress_photo(&original).unwrap();
    assert_eq!(fs::read(&original).unwrap(), unchanged);
    assert!(compressed.starts_with(&[0xff, 0xd8]));
    let destination = dir.join("compressed.jpg");
    fs::write(&destination, compressed).unwrap();
    let dimensions = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(&destination)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert!(dimensions.status.success());
    assert_eq!(
        String::from_utf8_lossy(&dimensions.stdout).trim(),
        "2560x640"
    );
    fs::remove_file(original).unwrap();
    fs::remove_file(destination).unwrap();
    fs::remove_dir(dir).unwrap();
}

fn raw_message_peer_id(message: &tl::enums::Message) -> Option<PeerId> {
    match message {
        tl::enums::Message::Empty(message) => message.peer_id.clone(),
        tl::enums::Message::Message(message) => Some(message.peer_id.clone()),
        tl::enums::Message::Service(message) => Some(message.peer_id.clone()),
    }
    .map(PeerId::from)
}

fn raw_message_ids(messages: tl::enums::messages::Messages) -> Vec<i32> {
    match messages {
        tl::enums::messages::Messages::Messages(messages) => messages.messages,
        tl::enums::messages::Messages::Slice(messages) => messages.messages,
        tl::enums::messages::Messages::ChannelMessages(messages) => messages.messages,
        tl::enums::messages::Messages::NotModified(_) => Vec::new(),
    }
    .into_iter()
    .map(|message| message.id())
    .collect()
}

fn text_with_entities(value: &tl::enums::TextWithEntities) -> &str {
    match value {
        tl::enums::TextWithEntities::Entities(value) => &value.text,
    }
}

fn presence(peer: &Peer) -> String {
    match peer {
        Peer::User(user) if user.is_bot() => "bot".to_owned(),
        Peer::User(user) => match user.status() {
            tl::enums::UserStatus::Online(_) => "online".to_owned(),
            tl::enums::UserStatus::Offline(status) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let elapsed = now.saturating_sub(i64::from(status.was_online));
                if elapsed < 60 {
                    "last seen just now".to_owned()
                } else if elapsed < 3600 {
                    format!("last seen {}m ago", elapsed / 60)
                } else if elapsed < 86_400 {
                    format!("last seen {}h ago", elapsed / 3600)
                } else {
                    format!("last seen {}d ago", elapsed / 86_400)
                }
            }
            tl::enums::UserStatus::Recently(_) => "last seen recently".to_owned(),
            tl::enums::UserStatus::LastWeek(_) => "last seen within a week".to_owned(),
            tl::enums::UserStatus::LastMonth(_) => "last seen within a month".to_owned(),
            tl::enums::UserStatus::Empty => "last seen unavailable".to_owned(),
        },
        Peer::Group(_) | Peer::Community(_) => "group".to_owned(),
        Peer::Channel(_) => "channel".to_owned(),
    }
}

fn matches_filter(chat: &Chat, filter: &tl::types::DialogFilter) -> bool {
    let peer_id = chat.peer.id;
    if filter
        .exclude_peers
        .iter()
        .any(|peer| input_peer_matches(peer, peer_id))
    {
        return false;
    }
    let explicitly_included = filter
        .include_peers
        .iter()
        .chain(&filter.pinned_peers)
        .any(|peer| input_peer_matches(peer, peer_id));
    if explicitly_included {
        return true;
    }
    if filter.exclude_archived && chat.archived || filter.exclude_read && chat.unread == 0 {
        return false;
    }
    match chat.kind {
        ChatKind::User if chat.is_bot => filter.bots,
        ChatKind::User if chat.is_contact => filter.contacts,
        ChatKind::User => filter.non_contacts,
        ChatKind::Group => filter.groups,
        ChatKind::Channel => filter.broadcasts,
    }
}

fn input_peer_matches(peer: &tl::enums::InputPeer, peer_id: PeerId) -> bool {
    let Some(id) = peer_id.bare_id() else {
        return matches!(peer, tl::enums::InputPeer::PeerSelf);
    };
    match (peer, peer_id.kind()) {
        (tl::enums::InputPeer::User(value), PeerKind::User) => value.user_id == id,
        (tl::enums::InputPeer::Chat(value), PeerKind::Chat) => value.chat_id == id,
        (tl::enums::InputPeer::Channel(value), PeerKind::Channel) => value.channel_id == id,
        _ => false,
    }
}

impl Drop for Telegram {
    fn drop(&mut self) {
        self.handle.quit();
    }
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value)
}
