use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use flate2::read::DeflateDecoder;
use grammers_client::Client;
use grammers_client::media::Document;
use grammers_client::tl;
use terminalib::{CellStyle, Rgb};

const MAX_THEME: usize = 5 * 1024 * 1024;

pub const LINK_PREFIXES: [&str; 7] = [
    "https://t.me/addtheme/",
    "http://t.me/addtheme/",
    "t.me/addtheme/",
    "https://telegram.me/addtheme/",
    "http://telegram.me/addtheme/",
    "telegram.me/addtheme/",
    "tg://addtheme?slug=",
];

fn theme_slug(source: &str) -> Option<&str> {
    LINK_PREFIXES
        .iter()
        .find_map(|prefix| source.strip_prefix(prefix))
        .map(|tail| tail.split(['?', '&', '/', '#']).next().unwrap_or_default())
        .filter(|slug| {
            !slug.is_empty() && slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

pub fn is_theme_link(source: &str) -> bool {
    theme_slug(source).is_some()
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub error: Rgb,
    pub border: Rgb,
    pub incoming: Rgb,
    pub outgoing: Rgb,
    pub window: Rgb,
    pub chat: Rgb,
    pub rail: Rgb,
}

#[derive(Clone)]
pub struct Theme {
    pub name: String,
    pub palette: Palette,
    pub transparent: bool,
    source: String,
    pub remote: Option<tl::enums::InputTheme>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: "Terminal".to_owned(),
            palette: Palette {
                accent: Rgb::new(82, 170, 242),
                text: Rgb::new(226, 232, 240),
                muted: Rgb::new(130, 145, 162),
                error: Rgb::new(248, 113, 113),
                border: Rgb::new(83, 101, 120),
                incoming: Rgb::new(226, 232, 240),
                outgoing: Rgb::new(226, 232, 240),
                window: Rgb::new(23, 33, 43),
                chat: Rgb::new(14, 22, 33),
                rail: Rgb::new(14, 22, 33),
            },
            transparent: true,
            source: String::new(),
            remote: None,
        }
    }
}

impl Theme {
    pub fn style(&self, color: Rgb, background: Rgb) -> CellStyle {
        CellStyle::new(Some(color), (!self.transparent).then_some(background))
    }

    pub fn builtin(name: &str) -> Option<Self> {
        let (title, source) = match name.to_ascii_lowercase().as_str() {
            "terminal" => return Some(Self::default()),
            "classic" => (
                "Classic",
                "windowBg: #ffffff; windowFg: #000000; windowSubTextFg: #999999; windowActiveTextFg: #40a7e3; historyTextInFg: #000000; historyTextOutFg: #000000; historyBg: #e6ebee; dialogsBg: #ffffff; dialogsMenuIconFg: #a0a0a0; sideBarBg: #f0f0f0;",
            ),
            "day" => (
                "Day",
                "windowBg: #ffffff; windowFg: #000000; windowSubTextFg: #999999; windowActiveTextFg: #40a7e3; historyTextInFg: #000000; historyTextOutFg: #000000; historyBg: #d7e3ec; dialogsBg: #ffffff; dialogsMenuIconFg: #a0a0a0; sideBarBg: #f0f0f0;",
            ),
            "tinted" => (
                "Tinted",
                "windowBg: #17212b; windowFg: #f5f5f5; windowSubTextFg: #708499; windowActiveTextFg: #5288c1; historyTextInFg: #f5f5f5; historyTextOutFg: #f5f5f5; historyBg: #0e1621; dialogsBg: #17212b; dialogsMenuIconFg: #708499; sideBarBg: #0e1621;",
            ),
            "night" => (
                "Night",
                "windowBg: #212121; windowFg: #f5f5f5; windowSubTextFg: #8f8f8f; windowActiveTextFg: #3fc1b0; historyTextInFg: #f5f5f5; historyTextOutFg: #f5f5f5; historyBg: #181818; dialogsBg: #212121; dialogsMenuIconFg: #8f8f8f; sideBarBg: #181818;",
            ),
            _ => return None,
        };
        Self::from_bytes(title, source.as_bytes()).ok()
    }

    pub fn from_bytes(name: &str, data: &[u8]) -> Result<Self> {
        if data.len() > MAX_THEME {
            bail!("Telegram theme exceeds 5 MiB");
        }
        let bytes = if data.starts_with(b"PK\x03\x04") {
            palette_from_zip(data)?
        } else {
            data.to_vec()
        };
        let source = std::str::from_utf8(&bytes).context("theme palette is not UTF-8")?;
        let values = parse_palette(source)?;
        let default = Self::default().palette;
        let get = |keys: &[&str], fallback| {
            keys.iter()
                .find_map(|key| resolve_color(key, &values, &mut HashSet::new()).ok())
                .unwrap_or(fallback)
        };
        let text = get(&["windowFg", "dialogsNameFg"], default.text);
        let window = get(&["dialogsBg", "windowBg"], default.window);
        Ok(Self {
            name: name.chars().filter(|c| !c.is_control()).take(60).collect(),
            palette: Palette {
                accent: get(&["windowActiveTextFg", "activeButtonBg"], default.accent),
                text,
                muted: get(&["dialogsTextFg", "windowSubTextFg"], default.muted),
                error: get(&["attentionButtonFg", "boxTextFgError"], default.error),
                border: get(&["dialogsMenuIconFg", "windowSubTextFg"], default.border),
                incoming: get(&["historyTextInFg", "windowFg"], text),
                outgoing: get(&["historyTextOutFg", "windowFg"], text),
                window,
                chat: get(&["historyBg", "windowBg"], window),
                rail: get(&["sideBarBg", "dialogsBg", "windowBg"], window),
            },
            transparent: false,
            source: source.to_owned(),
            remote: None,
        })
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(root.join("theme.tdesktop-palette"), &self.source)?;
        fs::write(
            root.join("theme-settings"),
            format!("{}\n{}\n", self.name, self.transparent),
        )?;
        Ok(())
    }

    pub fn restore(root: &Path) -> Self {
        let restored = (|| -> Result<Self> {
            let settings = fs::read_to_string(root.join("theme-settings"))?;
            let mut lines = settings.lines();
            let name = lines.next().unwrap_or("Theme");
            let transparent = lines.next() == Some("true");
            let bytes = fs::read(root.join("theme.tdesktop-palette"))?;
            let mut theme = if bytes.is_empty() {
                Self::default()
            } else {
                Self::from_bytes(name, &bytes)?
            };
            theme.transparent = transparent;
            Ok(theme)
        })();
        restored.unwrap_or_default()
    }
}

pub async fn load(client: &Client, source: &str) -> Result<Theme> {
    if let Some(theme) = Theme::builtin(source) {
        return Ok(theme);
    }
    if let Some(slug) = theme_slug(source) {
        let tl::enums::Theme::Theme(theme) = client
            .invoke(&tl::functions::account::GetTheme {
                format: "tdesktop".to_owned(),
                theme: tl::types::InputThemeSlug {
                    slug: slug.to_owned(),
                }
                .into(),
            })
            .await?;
        let raw = theme
            .document
            .context("theme has no Telegram Desktop palette")?;
        let document = Document::from_raw_media(tl::types::MessageMediaDocument {
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
        });
        let mut data = Vec::new();
        let mut download = client.iter_download(&document);
        while let Some(chunk) = download.next().await? {
            if data.len() + chunk.len() > MAX_THEME {
                bail!("Telegram theme exceeds 5 MiB");
            }
            data.extend(chunk);
        }
        let mut loaded = Theme::from_bytes(&theme.title, &data)?;
        loaded.remote = Some(
            tl::types::InputTheme {
                id: theme.id,
                access_hash: theme.access_hash,
            }
            .into(),
        );
        return Ok(loaded);
    }
    let path = if let Some(relative) = source.strip_prefix("~/") {
        dirs::home_dir()
            .context("home directory is unavailable")?
            .join(relative)
    } else {
        source.into()
    };
    let data = fs::read(&path).with_context(|| format!("cannot read theme {}", path.display()))?;
    Theme::from_bytes(
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("Theme"),
        &data,
    )
}

fn parse_palette(source: &str) -> Result<HashMap<String, String>> {
    let mut cleaned = String::new();
    let mut chars = source.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'/') {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            cleaned.push('\n');
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else {
            cleaned.push(c);
        }
    }
    let mut values = HashMap::new();
    for statement in cleaned.split(';').filter(|s| !s.trim().is_empty()) {
        let (key, value) = statement
            .split_once(':')
            .context("invalid Telegram palette statement")?;
        values.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    if values.is_empty() {
        bail!("theme palette is empty");
    }
    for key in values.keys() {
        validate_alias(key, &values, &mut HashSet::new())?;
    }
    Ok(values)
}

fn validate_alias(
    key: &str,
    values: &HashMap<String, String>,
    visiting: &mut HashSet<String>,
) -> Result<()> {
    if !visiting.insert(key.to_owned()) {
        bail!("cyclic palette alias: {key}");
    }
    if let Some(value) = values.get(key) {
        if let Some(hex) = value.strip_prefix('#') {
            if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("invalid palette color: {value}");
            }
        } else if values.contains_key(value) {
            validate_alias(value, values, visiting)?;
        }
        // Unspecified aliases inherit Telegram Desktop's base palette.
    }
    visiting.remove(key);
    Ok(())
}

fn resolve_color(
    key: &str,
    values: &HashMap<String, String>,
    visiting: &mut HashSet<String>,
) -> Result<Rgb> {
    if !visiting.insert(key.to_owned()) {
        bail!("cyclic palette alias: {key}");
    }
    let value = values
        .get(key)
        .with_context(|| format!("unknown palette color: {key}"))?;
    let color = if let Some(hex) = value.strip_prefix('#') {
        if !matches!(hex.len(), 6 | 8) || !hex.is_ascii() {
            bail!("invalid palette color: {value}");
        }
        Rgb::new(
            u8::from_str_radix(&hex[0..2], 16)?,
            u8::from_str_radix(&hex[2..4], 16)?,
            u8::from_str_radix(&hex[4..6], 16)?,
        )
    } else {
        resolve_color(value, values, visiting)?
    };
    visiting.remove(key);
    Ok(color)
}

// Read only the palette entry; never extract theme archives to the filesystem.
fn palette_from_zip(bytes: &[u8]) -> Result<Vec<u8>> {
    let end = (0..bytes.len().saturating_sub(3))
        .rev()
        .find(|&i| bytes[i..].starts_with(b"PK\x05\x06"))
        .context("invalid theme ZIP directory")?;
    let u16_at = |i| -> Result<usize> {
        Ok(u16::from_le_bytes(
            bytes
                .get(i..i + 2)
                .context("truncated theme ZIP")?
                .try_into()?,
        ) as usize)
    };
    let u32_at = |i| -> Result<usize> {
        Ok(u32::from_le_bytes(
            bytes
                .get(i..i + 4)
                .context("truncated theme ZIP")?
                .try_into()?,
        ) as usize)
    };
    let mut entry = u32_at(end + 16)?;
    for _ in 0..u16_at(end + 10)? {
        if bytes.get(entry..entry + 4) != Some(b"PK\x01\x02") {
            bail!("invalid theme ZIP entry");
        }
        let name_length = u16_at(entry + 28)?;
        let name = bytes
            .get(entry + 46..entry + 46 + name_length)
            .context("truncated ZIP filename")?;
        let name = std::str::from_utf8(name)?.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "colors.tdesktop-theme" | "colors.tdesktop-palette"
        ) {
            if u16_at(entry + 8)? & 1 != 0 {
                bail!("encrypted themes are not supported");
            }
            if u32_at(entry + 24)? > MAX_THEME {
                bail!("theme palette exceeds 5 MiB");
            }
            let local = u32_at(entry + 42)?;
            if bytes.get(local..local + 4) != Some(b"PK\x03\x04") {
                bail!("invalid local ZIP entry");
            }
            let start = local + 30 + u16_at(local + 26)? + u16_at(local + 28)?;
            let data = bytes
                .get(start..start + u32_at(entry + 20)?)
                .context("truncated ZIP payload")?;
            let mut output = Vec::new();
            match u16_at(entry + 10)? {
                0 => output.extend_from_slice(data),
                8 => {
                    DeflateDecoder::new(data)
                        .take((MAX_THEME + 1) as u64)
                        .read_to_end(&mut output)?;
                }
                _ => bail!("unsupported theme ZIP compression"),
            }
            if output.len() > MAX_THEME {
                bail!("theme palette exceeds 5 MiB");
            }
            return Ok(output);
        }
        entry += 46 + name_length + u16_at(entry + 30)? + u16_at(entry + 32)?;
    }
    bail!("archive has no colors.tdesktop-theme or colors.tdesktop-palette")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_aliases_comments_and_rgba() {
        let theme = Theme::from_bytes("Test", b"// palette\naccent: #123456ff; /* alias */ windowActiveTextFg: accent; windowFg: #abcdef;").unwrap();
        assert_eq!(theme.palette.accent, Rgb::new(0x12, 0x34, 0x56));
        assert_eq!(theme.palette.incoming, Rgb::new(0xab, 0xcd, 0xef));
    }

    #[test]
    fn rejects_cycles_and_invalid_archives() {
        assert!(Theme::from_bytes("Bad", b"a: b; b: a;").is_err());
        assert!(Theme::from_bytes("Bad", b"PK\x03\x04broken").is_err());
    }

    #[test]
    fn imports_deflated_desktop_theme_archives() {
        use std::io::Write;
        let name = b"colors.tdesktop-theme";
        let palette = b"windowFg: #abcdef; windowActiveTextFg: #123456;";
        let mut compressor =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        compressor.write_all(palette).unwrap();
        let data = compressor.finish().unwrap();
        let mut zip = vec![0u8; 30];
        zip[..4].copy_from_slice(b"PK\x03\x04");
        zip[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend(name);
        zip.extend(&data);
        let offset = zip.len();
        let mut central = vec![0u8; 46];
        central[..4].copy_from_slice(b"PK\x01\x02");
        central[10..12].copy_from_slice(&8u16.to_le_bytes());
        central[20..24].copy_from_slice(&(data.len() as u32).to_le_bytes());
        central[24..28].copy_from_slice(&(palette.len() as u32).to_le_bytes());
        central[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend(&central);
        zip.extend(name);
        let mut end = vec![0u8; 22];
        end[..4].copy_from_slice(b"PK\x05\x06");
        end[10..12].copy_from_slice(&1u16.to_le_bytes());
        end[16..20].copy_from_slice(&(offset as u32).to_le_bytes());
        zip.extend(end);
        let theme = Theme::from_bytes("Archive", &zip).unwrap();
        assert_eq!(theme.palette.text, Rgb::new(0xab, 0xcd, 0xef));
    }
}
