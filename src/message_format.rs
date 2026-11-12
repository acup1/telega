use grammers_client::{
    message::{InputMessage, Message},
    tl,
};

use crate::custom_emoji::{self, Draft};

/// Strip only complete fenced code blocks; keep all other user text verbatim.
pub fn message(source: &str, draft: &[Draft], premium: bool) -> InputMessage {
    let (text, entities) = formatted(source, draft, premium);
    InputMessage::new().text(text).fmt_entities(entities)
}

pub fn editable(message: &Message) -> (String, Vec<Draft>) {
    editable_text(
        message.text(),
        message
            .fmt_entities()
            .map(Vec::as_slice)
            .unwrap_or_default(),
    )
}

fn editable_text(text: &str, entities: &[tl::enums::MessageEntity]) -> (String, Vec<Draft>) {
    let mut source = text.to_owned();
    let mut draft: Vec<Draft> = entities
        .iter()
        .filter_map(|entity| {
            let (offset, length, id, free) = match entity {
                tl::enums::MessageEntity::CustomEmoji(e) => {
                    (e.offset, e.length, e.document_id, true)
                }
                tl::enums::MessageEntity::TextUrl(e) => (
                    e.offset,
                    e.length,
                    custom_emoji::compatible_id(&e.url)?,
                    false,
                ),
                _ => return None,
            };
            Some(Draft {
                offset,
                length,
                id,
                alt: custom_emoji::slice_utf16(text, offset, length)?.to_owned(),
                free,
            })
        })
        .collect();
    let mut blocks: Vec<_> = entities
        .iter()
        .filter_map(|entity| match entity {
            tl::enums::MessageEntity::Pre(pre) => Some(pre),
            _ => None,
        })
        .collect();
    blocks.sort_by_key(|pre| std::cmp::Reverse(pre.offset));
    for block in blocks {
        let Some(start) = utf16_byte(text, block.offset) else {
            continue;
        };
        let Some(end) = utf16_byte(text, block.offset + block.length) else {
            continue;
        };
        // Fences are line-oriented; preserve text exactly when a block is already line-oriented.
        if (start > 0 && !text[..start].ends_with('\n')) || !text[start..end].ends_with('\n') {
            continue;
        }
        let fence = "`".repeat(
            3.max(
                text[start..end]
                    .split(|c| c != '`')
                    .map(str::len)
                    .max()
                    .unwrap_or(0)
                    + 1,
            ),
        );
        let prefix = format!("{fence}{}\n", block.language);
        let suffix = format!("{fence}\n");
        source.replace_range(
            start..end,
            &format!("{prefix}{}{suffix}", &text[start..end]),
        );
        let delta = (prefix.encode_utf16().count() + suffix.encode_utf16().count()) as i32;
        draft.retain(|emoji| {
            emoji.offset + emoji.length <= block.offset
                || emoji.offset >= block.offset + block.length
        });
        for emoji in &mut draft {
            if emoji.offset >= block.offset + block.length {
                emoji.offset += delta;
            }
        }
    }
    (source, draft)
}

fn utf16_byte(text: &str, offset: i32) -> Option<usize> {
    if offset < 0 {
        return None;
    }
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += ch.len_utf16() as i32;
    }
    (units == offset).then_some(text.len())
}

fn fence(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let count = trimmed.bytes().take_while(|b| *b == b'`').count();
    (count >= 3).then(|| (count, trimmed[count..].trim()))
}

fn formatted(
    source: &str,
    draft: &[Draft],
    premium: bool,
) -> (String, Vec<tl::enums::MessageEntity>) {
    let mut text = String::new();
    let mut entities = Vec::new();
    let mut mapping = Vec::new();
    let lines: Vec<_> = source.split_inclusive('\n').collect();
    let mut offsets = Vec::new();
    let mut offset = 0;
    for line in &lines {
        offsets.push(offset);
        offset += line.len();
    }
    let mut i = 0;
    while i < lines.len() {
        if let Some((count, info)) = fence(lines[i])
            && !info.contains('`')
            && let Some(end) = (i + 1..lines.len())
                .find(|&j| fence(lines[j]).is_some_and(|(n, rest)| n >= count && rest.is_empty()))
        {
            let start = offsets[i] + lines[i].len();
            let body = &source[start..offsets[end]];
            let position = text.encode_utf16().count() as i32;
            text.push_str(body);
            if !body.is_empty() {
                entities.push(
                    tl::types::MessageEntityPre {
                        offset: position,
                        length: body.encode_utf16().count() as i32,
                        language: info
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_owned(),
                    }
                    .into(),
                );
            }
            // Keep the line boundary after an empty block, too.
            if body.is_empty() && lines[end].ends_with('\n') {
                text.push('\n');
            }
            i = end + 1;
        } else {
            // Telegram accepts one-line triple-backtick code as a Code entity.
            // A full fenced block above remains a Pre entity with its language.
            let line = lines[i];
            let mut cursor = 0;
            while let Some(open) = line[cursor..].find("```").map(|at| cursor + at) {
                let Some(close) = line[open + 3..].find("```").map(|at| open + 3 + at) else {
                    break;
                };
                append_plain(
                    source,
                    offsets[i] + cursor,
                    offsets[i] + open,
                    &mut text,
                    &mut mapping,
                );
                let body = &line[open + 3..close];
                if !body.is_empty() {
                    entities.push(
                        tl::types::MessageEntityCode {
                            offset: text.encode_utf16().count() as i32,
                            length: body.encode_utf16().count() as i32,
                        }
                        .into(),
                    );
                    text.push_str(body);
                }
                cursor = close + 3;
            }
            append_plain(
                source,
                offsets[i] + cursor,
                offsets[i] + line.len(),
                &mut text,
                &mut mapping,
            );
            i += 1;
        }
    }
    let remapped: Vec<_> = draft
        .iter()
        .filter_map(|emoji| {
            let &(start, length, target) = mapping.iter().find(|&&(start, length, _)| {
                emoji.offset >= start && emoji.offset + emoji.length <= start + length
            })?;
            let mut emoji = emoji.clone();
            emoji.offset += target - start;
            let _ = length;
            Some(emoji)
        })
        .collect();
    entities.extend(custom_emoji::outgoing(&text, &remapped, premium));
    (text, entities)
}

fn append_plain(
    source: &str,
    start: usize,
    end: usize,
    text: &mut String,
    mapping: &mut Vec<(i32, i32, i32)>,
) {
    if start == end {
        return;
    }
    mapping.push((
        source[..start].encode_utf16().count() as i32,
        source[start..end].encode_utf16().count() as i32,
        text.encode_utf16().count() as i32,
    ));
    text.push_str(&source[start..end]);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fences_become_pre_entities_with_utf16_offsets_and_preserved_code() {
        let (text, entities) = formatted("😀\n```rust\n  let x = 1;\n\n```\nafter", &[], false);
        assert_eq!(text, "😀\n  let x = 1;\n\nafter");
        assert!(matches!(&entities[0], tl::enums::MessageEntity::Pre(p)
            if p.offset == 3 && p.language == "rust" && custom_emoji::slice_utf16(&text,p.offset,p.length)==Some("  let x = 1;\n\n")));
        assert_eq!(
            formatted("```rs\nunclosed", &[], false).0,
            "```rs\nunclosed"
        );
        let (text, entities) = formatted("```\nx\n```", &[], false);
        assert_eq!(text, "x\n");
        assert!(matches!(&entities[0],tl::enums::MessageEntity::Pre(p) if p.language.is_empty()));
    }
    #[test]
    fn inline_triple_backticks_become_code_entities() {
        let (text, entities) = formatted("Привет ```let x = 1``` готово", &[], false);
        assert_eq!(text, "Привет let x = 1 готово");
        assert!(matches!(&entities[0], tl::enums::MessageEntity::Code(code)
            if custom_emoji::slice_utf16(&text, code.offset, code.length) == Some("let x = 1")));
        let (text, entities) = formatted("```x```", &[], false);
        assert_eq!(text, "x");
        assert!(matches!(&entities[0], tl::enums::MessageEntity::Code(_)));
    }
    #[test]
    fn custom_emoji_after_code_is_remapped_but_code_content_is_not_an_emoji_entity() {
        let source = "```\n😀\n```\n😀";
        let draft = [
            Draft {
                offset: 4,
                length: 2,
                id: 10,
                alt: "😀".into(),
                free: false,
            },
            Draft {
                offset: 11,
                length: 2,
                id: 11,
                alt: "😀".into(),
                free: false,
            },
        ];
        let (text, entities) = formatted(source, &draft, false);
        assert_eq!(text, "😀\n😀");
        assert_eq!(entities.len(), 2);
        assert!(matches!(&entities[1],tl::enums::MessageEntity::TextUrl(e) if e.offset==3));
    }

    #[test]
    fn editing_a_code_message_preserves_pre_and_custom_emoji_positions() {
        let body = "  let y=1;\n";
        let original = format!("x\n{body}😀");
        let pre: tl::enums::MessageEntity = tl::types::MessageEntityPre {
            offset: 2,
            length: body.encode_utf16().count() as i32,
            language: "rust".into(),
        }
        .into();
        let emoji: tl::enums::MessageEntity = tl::types::MessageEntityCustomEmoji {
            offset: (2 + body.encode_utf16().count()) as i32,
            length: 2,
            document_id: 42,
        }
        .into();
        let (editable, draft) = editable_text(&original, &[pre, emoji]);
        assert!(editable.contains("```rust\n"));
        let (roundtrip, entities) = formatted(&editable, &draft, true);
        assert_eq!(roundtrip, original);
        assert!(
            matches!(&entities[0],tl::enums::MessageEntity::Pre(p) if p.offset==2 && p.language=="rust")
        );
        assert!(
            matches!(&entities[1],tl::enums::MessageEntity::CustomEmoji(e) if e.offset==(2 + body.encode_utf16().count()) as i32 && e.document_id==42)
        );
    }
}
