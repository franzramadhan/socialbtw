//! Threads API: the access token, reading posts and replies, hiding replies.

use crate::http::Http;
use crate::metrics::metrics;
use crate::store::Store;
use crate::util::{log, now, query};
use serde_json::{json, Value};

pub const API: &str = "https://graph.threads.net/v1.0";

/// Threads calls with the token kept in the store.
pub struct Threads<'a> {
    pub http: &'a Http,
    pub store: &'a Store,
}

impl Threads<'_> {
    pub fn token(&self) -> String {
        self.store.get("token").unwrap_or_default()
    }

    /// THREADS_ACCESS_TOKEN seeds the store; changing it replaces the stored token.
    /// Without it, tokens come only from /oauth/start.
    pub fn seed(&self, seed: &str) {
        if !seed.is_empty() && self.store.get("seed").as_deref() != Some(seed) {
            self.store.put("seed", seed);
            self.store.put("token", seed);
            self.store.put("expires_at", "0");
        }
        metrics().token_expiry = self.store.get("expires_at").and_then(|v| v.parse().ok()).unwrap_or(0);
    }

    /// Swap the long-lived token for a new one (60 days from now). The token
    /// therefore never expires while the app runs at least once every 60 days.
    pub fn refresh(&self) {
        let url = format!(
            "https://graph.threads.net/refresh_access_token?{}",
            query(&[("grant_type", "th_refresh_token"), ("access_token", &self.token())])
        );
        let resp = (self.http)("refresh", "GET", &url, None, None);
        match resp.as_ref().map(|r| (r["access_token"].as_str(), r["expires_in"].as_u64())) {
            Ok((Some(token), Some(expires_in))) => {
                let expires_at = now() + expires_in;
                self.store.put("token", token);
                self.store.put("expires_at", &expires_at.to_string());
                metrics().token_expiry = expires_at;
                log(json!({"level": "info", "msg": "token refreshed", "expires_at": expires_at}));
            }
            // A token younger than 24h cannot be refreshed yet; the next attempt is tomorrow.
            other => {
                let error = other.err().cloned().unwrap_or_else(|| "refresh: response without access_token or expires_in".into());
                log(json!({"level": "warning", "msg": "token refresh failed", "error": error}));
            }
        }
    }

    fn call(&self, method: &str, path: &str, endpoint: &'static str, params: &[(&str, &str)]) -> Result<Value, String> {
        let token = self.token();
        let mut all = params.to_vec();
        all.push(("access_token", &token));
        (self.http)(endpoint, method, &format!("{API}/{path}?{}", query(&all)), None, None)
    }

    fn paged(&self, path: &str, endpoint: &'static str, params: &[(&str, &str)]) -> Result<Vec<Value>, String> {
        let mut page = self.call("GET", path, endpoint, params)?;
        let mut out = Vec::new();
        loop {
            out.extend(page["data"].as_array().cloned().unwrap_or_default());
            match page["paging"]["next"].as_str().map(String::from) {
                Some(next) => page = (self.http)(endpoint, "GET", &next, None, None)?,
                None => return Ok(out),
            }
        }
    }

    /// Your posts since `since` (Unix seconds), reposts included.
    pub fn posts(&self, since: u64) -> Result<Vec<Value>, String> {
        // limit=100 (default 25): fewer paging calls for accounts that post a lot
        self.paged(
            "me/threads",
            "posts",
            &[("fields", "id,text,timestamp,topic_tag,media_type"), ("since", &since.to_string()), ("limit", "100")],
        )
    }

    /// Top-level replies to one post.
    pub fn replies(&self, post_id: &str) -> Result<Vec<Value>, String> {
        self.paged(
            &format!("{post_id}/replies"),
            "replies",
            &[("fields", "id,text,username,permalink,timestamp,hide_status,is_reply_owned_by_me"), ("limit", "100")],
        )
    }

    /// Hide a top-level reply (and everything nested under it).
    pub fn hide(&self, reply_id: &str) -> Result<(), String> {
        self.call("POST", &format!("{reply_id}/manage_reply"), "hide", &[("hide", "true")]).map(|_| ())
    }
}

/// Threads timestamp ("2026-10-19T09:19:56+0000") to Unix seconds.
// ponytail: ignores the offset; Threads always answers in UTC (+0000).
pub fn parse_ts(s: &str) -> Option<u64> {
    let n = |a: usize, b: usize| -> Option<i64> { s.get(a..b)?.parse().ok() };
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    // days from civil date (Howard Hinnant's algorithm)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 86400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("2026-10-19T09:19:56+0000"), Some(1_792_401_596));
        assert_eq!(parse_ts("2024-02-29T23:59:59+0000"), Some(1_709_251_199));
        assert_eq!(parse_ts("garbage"), None);
    }
}
