//! Sending chat: a queue that the rest of the app writes to, and one task
//! that delivers it through Helix. Every message gets Twitch's quirks
//! handled in one place (500-character limit, duplicate-message filter).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::helix::{Helix, HelixError, Sent};
use crate::commands::truncate_chars;

/// Twitch silently drops a message identical to one this account sent recently,
/// and the window isn't something we can track reliably. A rotating cosmetic
/// suffix makes consecutive messages differ.
const DEDUP_SUFFIXES: [&str; 4] = [" \u{1F3B5}", " \u{1F3B6}", " \u{1F3A7}", " \u{1F50A}"];
const MAX_MESSAGE_CHARS: usize = 500;
/// Pause between sends so a burst of replies stays well under Twitch's rate limit.
const SEND_GAP: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub text: String,
    /// Message id to reply to; `None` posts a plain announcement.
    pub reply_to: Option<String>,
}

/// Where the rest of the app puts chat messages.
#[derive(Clone)]
pub struct ChatOut {
    tx: UnboundedSender<Outgoing>,
}

impl ChatOut {
    pub fn channel() -> (ChatOut, UnboundedReceiver<Outgoing>) {
        let (tx, rx) = unbounded_channel();
        (ChatOut { tx }, rx)
    }

    pub fn reply(&self, to_message_id: &str, text: impl Into<String>) {
        let _ = self.tx.send(Outgoing { text: text.into(), reply_to: Some(to_message_id.to_string()) });
    }

    pub fn announce(&self, text: impl Into<String>) {
        let _ = self.tx.send(Outgoing { text: text.into(), reply_to: None });
    }
}

/// Adds the rotating suffix and enforces the length limit (cutting the message, not the suffix).
pub fn decorate(text: &str, counter: &mut usize) -> String {
    let suffix = DEDUP_SUFFIXES[*counter % DEDUP_SUFFIXES.len()];
    *counter += 1;
    let budget = MAX_MESSAGE_CHARS - suffix.chars().count();
    format!("{}{suffix}", truncate_chars(text, budget))
}

/// Delivers queued messages until every `ChatOut` is dropped. Delivery failures
/// are logged and dropped: a message Twitch declines to show is not worth retrying.
pub async fn run_sender(mut rx: UnboundedReceiver<Outgoing>, helix: Arc<Helix>, broadcaster_id: String, bot_id: String) {
    let mut counter = 0usize;
    while let Some(out) = rx.recv().await {
        let text = decorate(&out.text, &mut counter);
        let (h, b, s) = (helix.clone(), broadcaster_id.clone(), bot_id.clone());
        let (to, shown) = (out.reply_to.clone(), text.clone());
        let result = tokio::task::spawn_blocking(move || h.send_chat_message(&b, &s, &text, to.as_deref())).await;
        match result {
            Ok(Ok(Sent::Delivered)) => {}
            Ok(Ok(Sent::Dropped(why))) => crate::info!("Twitch didn't post a message ({why}): {shown:?}"),
            Ok(Err(HelixError::RateLimited)) => {
                crate::warn!("Twitch rate-limited the bot; backing off before the next message.");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Ok(Err(e)) => crate::info!("Chat message not delivered ({e}): {shown:?}"),
            Err(_) => crate::warn!("the chat sender crashed on a message"),
        }
        tokio::time::sleep(SEND_GAP).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_messages_never_end_the_same_way() {
        let mut n = 0;
        let a = decorate("Skipped.", &mut n);
        let b = decorate("Skipped.", &mut n);
        let c = decorate("Skipped.", &mut n);
        assert!(a != b && b != c && a.starts_with("Skipped."));
        // The suffix cycle repeats every four messages.
        let mut m = 0;
        let first = decorate("x", &mut m);
        for _ in 0..3 {
            decorate("x", &mut m);
        }
        assert_eq!(decorate("x", &mut m), first);
    }

    #[test]
    fn long_messages_are_cut_to_the_limit_with_the_suffix_intact() {
        let mut n = 0;
        let out = decorate(&"é".repeat(2_000), &mut n);
        assert_eq!(out.chars().count(), MAX_MESSAGE_CHARS);
        assert!(out.ends_with(DEDUP_SUFFIXES[0]) && out.contains('\u{2026}'));
        assert_eq!(decorate("short", &mut n).chars().count(), "short".len() + 2);
    }

    #[test]
    fn replies_and_announcements_are_queued_in_order() {
        let (chat, mut rx) = ChatOut::channel();
        chat.reply("m1", "hello");
        chat.announce("now playing");
        assert_eq!(rx.try_recv().unwrap(), Outgoing { text: "hello".into(), reply_to: Some("m1".into()) });
        assert_eq!(rx.try_recv().unwrap(), Outgoing { text: "now playing".into(), reply_to: None });
    }
}
