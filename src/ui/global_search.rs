use super::*;
use grammers_client::peer::User;

impl App {
    pub(super) fn search_changed(&mut self) {
        self.global_search_generation = self.global_search_generation.wrapping_add(1);
        self.remote_results.clear();
        self.global_search_due = (self.search_query.trim().chars().count() >= 2)
            .then(|| Instant::now() + Duration::from_millis(250));
        self.global_search_loading = self.global_search_due.is_some();
    }

    pub(super) fn poll_global_search(&mut self) {
        if !self
            .global_search_due
            .is_some_and(|due| Instant::now() >= due)
        {
            return;
        }
        self.global_search_due = None;
        let client = self.telegram.client.clone();
        let query = self.search_query.trim().to_owned();
        let generation = self.global_search_generation;
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let response: tl::types::contacts::Found = client
                    .invoke(&tl::functions::contacts::Search {
                        broadcasts: true,
                        bots: true,
                        q: query,
                        limit: 40,
                    })
                    .await?
                    .into();
                let mut peers = HashMap::new();
                for raw in response.users {
                    let reference = PeerRef::from(raw.clone());
                    let object = Peer::User(User::from_raw(&client, raw));
                    peers.insert(reference.id, Chat::from_search_peer(object, reference));
                }
                for raw in response.chats {
                    let reference = PeerRef::from(raw.clone());
                    let object = Peer::from_raw(&client, raw);
                    peers.insert(reference.id, Chat::from_search_peer(object, reference));
                }
                let mut seen = HashSet::new();
                Ok(response
                    .my_results
                    .into_iter()
                    .chain(response.results)
                    .filter_map(|raw| {
                        let id = PeerId::from(raw);
                        seen.insert(id).then(|| peers.remove(&id)).flatten()
                    })
                    .collect::<Vec<_>>())
            }
            .await
            .map_err(|error: grammers_client::InvocationError| error.to_string());
            let _ = tx.send(UiEvent::GlobalSearchReady { generation, result });
        });
    }

    pub(super) fn global_search_event(
        &mut self,
        generation: u64,
        result: std::result::Result<Vec<Chat>, String>,
    ) {
        if generation != self.global_search_generation {
            return;
        }
        self.global_search_loading = false;
        match result {
            Ok(results) => {
                self.remote_results.clear();
                for chat in results {
                    let id = chat.peer.id;
                    self.remote_results.insert(id);
                    if !self.chats.iter().any(|existing| existing.peer.id == id) {
                        self.chats.push(chat);
                    }
                }
                self.queue_avatars();
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(super) fn open_remote_peer(&mut self, chat: Chat) {
        let index = if let Some(index) = self
            .chats
            .iter()
            .position(|old| old.peer.id == chat.peer.id)
        {
            index
        } else {
            let index = self.chats.len();
            self.chats.push(chat);
            self.queue_avatars();
            index
        };
        self.chat_index = index;
        self.searching = false;
        self.open_chat(index);
    }

    pub(super) fn resolve_mention(&mut self, username: String) {
        self.mention_request = self.mention_request.wrapping_add(1);
        let generation = self.mention_request;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = match client.resolve_username(&username).await {
                Ok(Some(peer)) => match peer.to_ref().await {
                    Ok(Some(reference)) => Ok(Some(Chat::from_search_peer(peer, reference))),
                    Ok(None) => Ok(None),
                    Err(error) => Err(error.to_string()),
                },
                Ok(None) => Ok(None),
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(UiEvent::MentionResolved {
                generation,
                result: Box::new(result),
            });
        });
    }
}
