//! Optional, read-only Telegram smoke test. Run with a copy of a logged-in
//! session: TELEGA_LIVE_SESSION=/path/to/copy cargo test --test live_readonly -- --ignored

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use grammers_client::{Client, tl};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;

#[tokio::test]
#[ignore = "requires an authenticated Telegram session and network access"]
async fn stories_search_and_language_pack_respond() {
    let path = std::env::var("TELEGA_LIVE_SESSION").expect("set TELEGA_LIVE_SESSION");
    let api_id: i32 = match option_env!("TELEGA_BUILD_TG_API_ID") {
        Some(value) => value.parse().expect("numeric TG_API_ID"),
        None => panic!("build with TG_API_ID"),
    };
    let session = Arc::new(
        SqliteSession::open(Path::new(&path))
            .await
            .expect("open copied session"),
    );
    let SenderPool { runner, handle, .. } = SenderPool::new(session, api_id);
    tokio::spawn(runner.run());
    let client = Client::new(handle);
    let request = async {
        assert!(client.is_authorized().await.expect("check authorization"));
        let me = client.get_me().await.expect("load self profile");
        let reference = me
            .to_ref()
            .await
            .expect("resolve self reference")
            .expect("self reference is available");
        if let Some(username) = me.username() {
            let resolved = client
                .resolve_username(username)
                .await
                .expect("resolve own username")
                .expect("own username exists");
            assert_eq!(resolved.id(), me.id());
        }
        let mut story_state = None;
        for _ in 0..100 {
            let response = client
                .invoke(&tl::functions::stories::GetAllStories {
                    next: story_state.is_some(),
                    hidden: false,
                    state: story_state.clone(),
                })
                .await
                .expect("load story list page");
            let tl::enums::stories::AllStories::Stories(page) = response else {
                break;
            };
            if !page.has_more || story_state.as_deref() == Some(page.state.as_str()) {
                story_state = None;
                break;
            }
            story_state = Some(page.state);
        }
        assert!(story_state.is_none(), "story pagination did not converge");
        client
            .invoke(&tl::functions::contacts::Search {
                broadcasts: true,
                bots: true,
                q: "telega-readonly-smoke-nonexistent".into(),
                limit: 1,
            })
            .await
            .expect("search remote peers");
        client
            .invoke(&tl::functions::langpack::GetLanguages {
                lang_pack: "tdesktop".into(),
            })
            .await
            .expect("load Telegram Desktop languages");
        let mut post_offset = 0;
        let mut seen_posts = std::collections::HashSet::new();
        for _ in 0..100 {
            let page: tl::types::stories::Stories = client
                .invoke(&tl::functions::stories::GetPinnedStories {
                    peer: reference.into(),
                    offset_id: post_offset,
                    limit: 30,
                })
                .await
                .expect("load own profile posts page")
                .into();
            let ids: Vec<_> = page.stories.iter().map(|story| story.id()).collect();
            if ids.is_empty() {
                break;
            }
            let next = *ids.last().expect("nonempty page");
            assert_ne!(
                post_offset, next,
                "profile post pagination stopped advancing"
            );
            for id in ids {
                seen_posts.insert(id);
            }
            post_offset = next;
            if seen_posts.len() >= page.count.max(0) as usize {
                break;
            }
        }
        let mut gift_offset = String::new();
        for _ in 0..100 {
            let page: tl::types::payments::SavedStarGifts = client
                .invoke(&tl::functions::payments::GetSavedStarGifts {
                    exclude_unsaved: false,
                    exclude_saved: false,
                    exclude_unlimited: false,
                    exclude_unique: false,
                    sort_by_value: false,
                    exclude_upgradable: false,
                    exclude_unupgradable: false,
                    peer_color_available: false,
                    exclude_hosted: false,
                    peer: reference.into(),
                    collection_id: None,
                    offset: gift_offset.clone(),
                    limit: 30,
                })
                .await
                .expect("load own profile gifts page")
                .into();
            let Some(next) = page.next_offset else {
                break;
            };
            assert_ne!(gift_offset, next, "gift pagination stopped advancing");
            gift_offset = next;
        }
        let pack: tl::types::LangPackDifference = client
            .invoke(&tl::functions::langpack::GetLangPack {
                lang_pack: "tdesktop".into(),
                lang_code: "ru".into(),
            })
            .await
            .expect("load Russian language pack")
            .into();
        assert!(pack.strings.iter().any(|item| {
            matches!(item, tl::enums::LangPackString::String(value) if value.key == "lng_menu_settings")
        }));
    };
    tokio::time::timeout(Duration::from_secs(45), request)
        .await
        .expect("read-only Telegram requests timed out");
}
