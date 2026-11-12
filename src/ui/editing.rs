use super::*;

pub(super) struct State {
    pub original: Message,
    saved_input: String,
    saved_custom: Vec<crate::custom_emoji::Draft>,
    saved_reply: Option<i32>,
    saved_cursor: composer::State,
    initial_input: String,
    initial_custom: Vec<crate::custom_emoji::Draft>,
}

impl App {
    pub(super) async fn submit_edit(&mut self) {
        let Some(edit) = &self.editing else { return };
        if self.input == edit.initial_input && self.draft_custom == edit.initial_custom {
            self.cancel_edit();
            self.status = "Unchanged".into();
            return;
        }
        let original = edit.original.clone();
        let text = self.input.clone();
        let input =
            crate::message_format::message(&text, &self.draft_custom, self.emoji_state.premium());
        self.status = "Editing…".into();
        match original.edit(input).await {
            Ok(()) => {
                let peer = self.current_peer();
                let fresh = if let Some(peer) = peer {
                    self.telegram
                        .client
                        .get_messages_by_id(peer, &[original.id()])
                        .await
                        .ok()
                        .and_then(|mut messages| messages.pop().flatten())
                } else {
                    None
                };
                if let (Some(index), Some(message)) =
                    (self.message_index_by_id(original.id()), fresh)
                {
                    self.messages[index] = message;
                }
                self.cancel_edit();
                self.status = "Edited".into();
                self.status_error = false;
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(super) fn begin_edit(&mut self, message: Message) {
        if !message.outgoing() || message.text().is_empty() {
            self.status = "This message cannot be edited".into();
            return;
        }
        // Switching the edited message must keep the original unsent draft,
        // rather than saving the previous message's edit as that draft.
        self.cancel_edit();
        let (source, custom) = crate::message_format::editable(&message);
        self.editing = Some(State {
            original: message.clone(),
            saved_input: std::mem::take(&mut self.input),
            saved_custom: std::mem::take(&mut self.draft_custom),
            saved_reply: self.reply_to.take(),
            saved_cursor: std::mem::take(&mut self.composer),
            initial_input: source.clone(),
            initial_custom: custom.clone(),
        });
        self.input = source;
        self.draft_custom = custom;
        self.composer = composer::State::default();
        self.composing = true;
        self.message_menu = None;
    }

    pub(super) fn cancel_edit(&mut self) {
        if let Some(edit) = self.editing.take() {
            self.input = edit.saved_input;
            self.draft_custom = edit.saved_custom;
            self.reply_to = edit.saved_reply;
            self.composer = edit.saved_cursor;
        }
    }

    pub(super) fn edit_previous(&mut self) {
        if self.editing.is_some() || self.edit_previous_pending || !self.can_compose() {
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        self.edit_previous_pending = true;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let mut search = client.search_messages(peer).sent_by_self();
            let result = async {
                for _ in 0..30 {
                    let Some(message) = search.next().await? else {
                        break;
                    };
                    if !message.text().is_empty() && message.outgoing() {
                        return Ok(Some(Box::new(message)));
                    }
                }
                Ok(None)
            }
            .await
            .map_err(|error: grammers_client::InvocationError| error.to_string());
            let _ = tx.send(UiEvent::EditPrevious { view_id, result });
        });
    }
}
