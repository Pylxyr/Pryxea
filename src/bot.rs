//! Turns chat messages into station actions and replies. Five commands, no
//! framework: `!sr`, `!skip`, `!nowplaying`, `!sq`, `!radio`.

use std::sync::Arc;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::commands::{self, Command, RadioArg};
use crate::station::{Replier, SkipOutcome, Station, Who};
use crate::twitch::chat::ChatOut;
use crate::twitch::eventsub::ChatMessage;

pub struct Bot {
    station: Arc<Station>,
    chat: ChatOut,
    /// The bot's own account: its messages are never treated as commands.
    bot_id: String,
}

impl Bot {
    pub fn new(station: Arc<Station>, chat: ChatOut, bot_id: String) -> Bot {
        Bot { station, chat, bot_id }
    }

    /// Handles chat until the sender is dropped.
    pub async fn run(self, mut messages: UnboundedReceiver<ChatMessage>) {
        while let Some(message) = messages.recv().await {
            self.handle(&message);
        }
    }

    pub fn handle(&self, msg: &ChatMessage) {
        if msg.chatter_id == self.bot_id {
            return;
        }
        let Some(command) = commands::parse(&msg.text) else { return };
        // Twitch ids are numeric; anything else isn't a real chatter.
        let Ok(id) = msg.chatter_id.parse::<u64>() else {
            self.chat.reply(&msg.message_id, "Couldn't identify you \u{2014} try again.");
            return;
        };
        let name = if msg.display_name.is_empty() { msg.login.clone() } else { msg.display_name.clone() };
        let reply = |text: String| self.chat.reply(&msg.message_id, text);
        match command {
            Command::Sr(query) => {
                let who = Who { id, name };
                reply(self.station.request(&who, &query, Replier { chat: self.chat.clone(), to: msg.message_id.clone() }));
            }
            Command::Skip => reply(match self.station.skip(id, msg.is_mod) {
                SkipOutcome::Skipped => "Skipped.".to_string(),
                SkipOutcome::NothingPlaying => commands::NOTHING_PLAYING.to_string(),
                SkipOutcome::NotYours => commands::SKIP_OWN_ONLY.to_string(),
            }),
            Command::NowPlaying => reply(match self.station.now_playing() {
                None => commands::NOTHING_PLAYING.to_string(),
                Some(np) => commands::now_playing(&np.title, &np.requester_name, np.started_at.elapsed().as_secs()),
            }),
            Command::Sq => {
                let titles = self.station.queued_titles();
                reply(commands::queue_summary(titles.iter().map(String::as_str)));
            }
            Command::Radio(arg) => reply(match arg {
                RadioArg::Status => commands::radio_status(self.station.radio_status()),
                RadioArg::Invalid => commands::USAGE_RADIO.to_string(),
                RadioArg::On | RadioArg::Off if !msg.is_mod => commands::RADIO_MODS_ONLY.to_string(),
                RadioArg::On | RadioArg::Off => {
                    let on = arg == RadioArg::On;
                    self.station.set_radio(on);
                    commands::radio_changed(on)
                }
            }),
        }
    }
}
