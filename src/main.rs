mod clipboard;
mod custom_emoji;
mod media;
mod message_format;
mod telegram;
mod theme;
mod ui;
mod video_source;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::telegram::Telegram;

static TUI_STARTED: AtomicBool = AtomicBool::new(false);
static TUI_LOG: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn child_stderr() -> std::process::Stdio {
    TUI_LOG
        .get()
        .and_then(|path| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        })
        .map(std::process::Stdio::from)
        .unwrap_or_else(std::process::Stdio::null)
}

#[derive(Debug)]
struct Config {
    api_id: i32,
    api_hash: Option<String>,
    session: PathBuf,
    root: PathBuf,
    runtime: PathBuf,
    avatars: PathBuf,
    log: PathBuf,
}

impl Config {
    fn from_env() -> Result<Self> {
        let local_credentials = fs::read_to_string("api-credentials.env").unwrap_or_default();
        let api_id = credential_value(
            "TG_API_ID",
            std::env::var("TG_API_ID").ok(),
            option_env!("TELEGA_BUILD_TG_API_ID"),
            &local_credentials,
        )
        .context("TG_API_ID is not set (create an application at https://my.telegram.org)")?
        .parse()
        .context("TG_API_ID must be a number")?;
        let api_hash = credential_value(
            "TG_API_HASH",
            std::env::var("TG_API_HASH").ok(),
            option_env!("TELEGA_BUILD_TG_API_HASH"),
            &local_credentials,
        )
        .filter(|value| !value.trim().is_empty());

        let root = std::env::var_os("TELEGA_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join(".cache/telega")
            });
        let session = std::env::var_os("TELEGA_SESSION")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("telega.session"));
        let runtime = root.join("runtime").join(std::process::id().to_string());
        let avatars = root.join("avatars");
        let log = root.join("telega.log");
        Ok(Self {
            api_id,
            api_hash,
            session,
            root,
            runtime,
            avatars,
            log,
        })
    }
}

fn credential_value(
    key: &str,
    environment: Option<String>,
    embedded: Option<&str>,
    local: &str,
) -> Option<String> {
    environment
        .or_else(|| {
            embedded
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            local.lines().find_map(|line| {
                let (name, value) = line.split_once('=')?;
                (name.trim() == key).then(|| value.trim().to_owned())
            })
        })
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        if TUI_STARTED.load(Ordering::Relaxed) {
            let path = TUI_LOG.get().cloned().unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(std::env::temp_dir)
                    .join(".cache/telega/telega.log")
            });
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut log) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                use std::io::Write;
                let _ = writeln!(log, "{error:#}");
            }
        } else {
            eprintln!("{error:#}");
        }
    }
}

async fn run() -> Result<()> {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    let config = Config::from_env()?;
    fs::create_dir_all(&config.root)?;
    migrate_legacy_storage(&config)?;
    cleanup_stale_runtime(&config.root)?;
    if let Some(parent) = config.session.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&config.runtime)?;
    fs::create_dir_all(&config.avatars)?;

    let telegram = Telegram::connect(config.api_id, &config.session).await?;
    telegram.authorize(config.api_hash.as_deref()).await?;
    let _ = TUI_LOG.set(config.log.clone());
    TUI_STARTED.store(true, Ordering::Relaxed);
    let result = ui::run(telegram, config.runtime.clone(), config.avatars, config.log).await;
    let _ = fs::remove_dir_all(config.runtime);
    result
}

fn cleanup_stale_runtime(root: &Path) -> Result<()> {
    let runtime = root.join("runtime");
    let Ok(entries) = fs::read_dir(runtime) else {
        return Ok(());
    };
    let current_pid = std::process::id();
    for entry in entries {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if pid != current_pid && !process_is_running(pid) && entry.path().is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn migrate_legacy_storage(config: &Config) -> Result<()> {
    let legacy_session = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("telega/telega.session");
    if config.session != legacy_session && !config.session.exists() && legacy_session.exists() {
        move_file(&legacy_session, &config.session)?;
        for suffix in ["-wal", "-shm"] {
            let legacy_sidecar = PathBuf::from(format!("{}{}", legacy_session.display(), suffix));
            if legacy_sidecar.exists() {
                let destination = PathBuf::from(format!("{}{}", config.session.display(), suffix));
                move_file(&legacy_sidecar, &destination)?;
            }
        }
    }

    let legacy_media = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("telega/media");
    if legacy_media == config.root || !legacy_media.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(&config.avatars)?;
    for entry in fs::read_dir(&legacy_media)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("avatar-") && entry.path().is_file() {
            move_file(&entry.path(), &config.avatars.join(name))?;
        }
    }
    fs::remove_dir_all(legacy_media)?;
    Ok(())
}

fn move_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    if destination.exists() {
        fs::remove_file(source)?;
        return Ok(());
    }
    if fs::rename(source, destination).is_err() {
        fs::copy(source, destination)?;
        fs::remove_file(source)?;
    }
    Ok(())
}

fn print_help() {
    println!(
        "telega — terminal Telegram client\n\n\
         Telegram application (optional when embedded at build time):\n  TG_API_ID       application id from my.telegram.org\n  \
         TG_API_HASH     application hash; required only for a new login\n\n\
         Optional environment:\n  TELEGA_SESSION  session database path\n  \
         TELEGA_CACHE    storage root (default: ~/.cache/telega)\n  \
         TELEGA_THEME    theme path, t.me/addtheme link, terminal/classic/day/tinted/night\n  \
         TELEGA_NERD_FONTS  0 disables Nerd Font icons (default: 1)\n\n\
         Run:\n  nix run .\n\n\
         Keys:\n  ↑/↓ or j/k  select   Enter open chat/media   Esc back\n  \
         i compose      a attachments          / search\n  \
         Ctrl+V paste images/files/text        p pinned messages\n  \
         Ctrl+E emoji/stickers/custom picker   click Copy above code blocks\n  \
         Enter send (keep focus)   Shift+Enter newline   Esc leave input\n  \
         c comments     g linked chat          m settings  t import theme\n  \
         r refresh      Esc/q back; quit from All chats without an open chat\n  \
         Mouse:\n  right click message menu · double click reply · click avatar in profile for photos"
    );
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn local_credentials_work_when_git_flakes_omit_the_ignored_build_file() {
        let local = "TG_API_ID=42\nTG_API_HASH=local-hash\n";
        assert_eq!(
            credential_value("TG_API_ID", None, Some(""), local).as_deref(),
            Some("42")
        );
        assert_eq!(
            credential_value("TG_API_HASH", None, None, local).as_deref(),
            Some("local-hash")
        );
        assert_eq!(
            credential_value("TG_API_ID", Some("99".into()), Some("55"), local).as_deref(),
            Some("99")
        );
        assert_eq!(
            credential_value("TG_API_ID", None, Some("55"), local).as_deref(),
            Some("55")
        );
        assert!(credential_value("TG_API_ID", None, None, "# TG_API_ID=42\nOTHER=42").is_none());
    }
}
