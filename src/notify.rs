//! Push notifications for replies that need you (review, block_suggested):
//! ntfy or Telegram, chosen by the shape of NOTIFY_URL.

use crate::http::Http;
use crate::util::{fatal, log, var};
use serde_json::{json, Value};

pub struct Notifier {
    url: String,
    token: String,
}

impl Notifier {
    /// NOTIFY_URL: an ntfy topic (https://ntfy.sh/my-topic) or a Telegram
    /// sendMessage URL with ?chat_id=. NOTIFY_TOKEN: optional ntfy access token.
    pub fn from_env() -> Option<Self> {
        let url = var("NOTIFY_URL", Some(""));
        if url.is_empty() {
            return None;
        }
        if request(&url, "", "", "").is_none() {
            fatal("invalid NOTIFY_URL, expected like https://ntfy.sh/my-topic or https://api.telegram.org/bot<token>/sendMessage?chat_id=<id>");
        }
        Some(Notifier { url, token: var("NOTIFY_TOKEN", Some("")) })
    }

    /// Best effort: a failed notification is logged (and counted in
    /// threads_api_errors_total{endpoint="notify"}), never retried.
    pub fn send(&self, http: &Http, title: &str, message: &str, click: &str) {
        let Some((url, body)) = request(&self.url, title, message, click) else { return };
        let token = Some(self.token.as_str()).filter(|t| !t.is_empty());
        if let Err(e) = http("notify", "POST", &url, token, Some(&body)) {
            log(json!({"level": "warning", "msg": "notify failed", "error": e}));
        }
    }
}

/// (URL, JSON body) for one notification; None for a NOTIFY_URL of neither shape.
fn request(url: &str, title: &str, message: &str, click: &str) -> Option<(String, Value)> {
    if let Some((base, chat_id)) = url.split_once("?chat_id=") {
        let text = [title, message, click].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n\n");
        return Some((base.into(), json!({"chat_id": chat_id, "text": text, "link_preview_options": {"is_disabled": true}})));
    }
    // ntfy publishes JSON to the server root, with the topic in the body.
    let (base, topic) = url.trim_end_matches('/').rsplit_once('/')?;
    if !base.contains("://") || topic.is_empty() {
        return None;
    }
    let mut body = json!({"topic": topic, "title": title, "message": message});
    if !click.is_empty() {
        body["click"] = json!(click);
    }
    Some((format!("{base}/"), body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests() {
        let (url, body) = request("https://ntfy.sh/btw-alerts", "Review", "slot gacor", "https://threads.com/p").unwrap();
        assert_eq!(url, "https://ntfy.sh/");
        assert_eq!(body, json!({"topic": "btw-alerts", "title": "Review", "message": "slot gacor", "click": "https://threads.com/p"}));
        let (url, body) = request("https://api.telegram.org/botT/sendMessage?chat_id=-100", "Review", "hi", "").unwrap();
        assert_eq!(url, "https://api.telegram.org/botT/sendMessage");
        assert_eq!((body["chat_id"].as_str(), body["text"].as_str()), (Some("-100"), Some("Review\n\nhi")));
        assert!(request("ntfy.sh", "", "", "").is_none());
        assert!(request("https://ntfy.sh/", "", "", "").is_none());
    }
}
