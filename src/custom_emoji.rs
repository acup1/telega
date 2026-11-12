use grammers_client::{message::Message, tl};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Draft {
    pub offset: i32,
    pub length: i32,
    pub id: i64,
    pub alt: String,
    pub free: bool,
}

pub fn slice_utf16(text: &str, offset: i32, length: i32) -> Option<&str> {
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(length).ok()?)?;
    let mut units = 0;
    let mut first = None;
    for (byte, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), '\0')))
    {
        if units == start {
            first = Some(byte);
        }
        if units == end {
            return first.map(|first| &text[first..byte]);
        }
        units += character.len_utf16();
    }
    None
}

pub fn compatible_id(url: &str) -> Option<i64> {
    let id = url
        .strip_prefix("tg://emoji?id=")?
        .split('&')
        .next()?
        .parse()
        .ok()?;
    (id > 0).then_some(id)
}

pub fn entities(message: &Message) -> Vec<(i32, i32, i64)> {
    message
        .fmt_entities()
        .into_iter()
        .flatten()
        .filter_map(|entity| match entity {
            tl::enums::MessageEntity::CustomEmoji(emoji) => {
                Some((emoji.offset, emoji.length, emoji.document_id))
            }
            tl::enums::MessageEntity::TextUrl(link) => {
                compatible_id(&link.url).map(|id| (link.offset, link.length, id))
            }
            _ => None,
        })
        .collect()
}

pub fn outgoing(text: &str, draft: &[Draft], premium: bool) -> Vec<tl::enums::MessageEntity> {
    draft
        .iter()
        .filter(|emoji| {
            emoji.id > 0
                && slice_utf16(text, emoji.offset, emoji.length) == Some(emoji.alt.as_str())
        })
        .map(|emoji| {
            if premium || emoji.free {
                tl::types::MessageEntityCustomEmoji {
                    offset: emoji.offset,
                    length: emoji.length,
                    document_id: emoji.id,
                }
                .into()
            } else {
                tl::types::MessageEntityTextUrl {
                    offset: emoji.offset,
                    length: emoji.length,
                    url: format!("tg://emoji?id={}", emoji.id),
                }
                .into()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_is_visible_text_with_an_id_and_uses_utf16_offsets() {
        let text = "Привет 😀 👨‍💻";
        let alt = "👨‍💻";
        let draft = [Draft {
            offset: "Привет 😀 ".encode_utf16().count() as i32,
            length: alt.encode_utf16().count() as i32,
            id: 12345,
            alt: alt.to_owned(),
            free: false,
        }];
        let fallback = outgoing(text, &draft, false);
        assert!(
            matches!(&fallback[0], tl::enums::MessageEntity::TextUrl(link) if compatible_id(&link.url) == Some(12345))
        );
        assert!(
            matches!(&outgoing(text, &draft, true)[0], tl::enums::MessageEntity::CustomEmoji(emoji) if emoji.document_id == 12345)
        );
        assert!(outgoing("changed", &draft, false).is_empty());
        assert!(slice_utf16("😀", 1, 1).is_none());
        assert!(compatible_id("https://example.com/?id=12345").is_none());
        assert!(compatible_id("tg://emoji?id=-1").is_none());
        let mut free = draft[0].clone();
        free.free = true;
        assert!(matches!(
            &outgoing(text, &[free], false)[0],
            tl::enums::MessageEntity::CustomEmoji(_)
        ));
    }
}
