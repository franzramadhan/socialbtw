//! Threads OAuth: register a new long-lived token from the browser.

use crate::http::Http;
use crate::metrics::metrics;
use crate::store::Store;
use crate::threads::API;
use crate::util::{log, now, query, var};
use serde_json::json;

/// With all of THREADS_APP_ID, THREADS_APP_SECRET and OAUTH_REDIRECT_URI set,
/// /oauth/start and /oauth/callback register a new token.
pub struct OAuth {
    pub app_id: String,
    pub app_secret: String,
    pub redirect_uri: String,
    /// Expected Threads account; a token for anyone else is rejected. Empty: any.
    pub username: String,
}

impl OAuth {
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| var(k, Some(""));
        let (app_id, app_secret, redirect_uri) = (get("THREADS_APP_ID"), get("THREADS_APP_SECRET"), get("OAUTH_REDIRECT_URI"));
        if app_id.is_empty() || app_secret.is_empty() || redirect_uri.is_empty() {
            return None;
        }
        Some(OAuth { app_id, app_secret, redirect_uri, username: get("THREADS_USERNAME").trim_start_matches('@').into() })
    }

    pub fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://threads.com/oauth/authorize?{}",
            query(&[
                ("client_id", &self.app_id),
                ("redirect_uri", &self.redirect_uri),
                ("scope", "threads_basic,threads_read_replies,threads_manage_replies"),
                ("response_type", "code"),
                ("state", state),
            ])
        )
    }

    /// Code -> short-lived token -> long-lived token (60 days), checked against
    /// `username`, then stored where the poller and the daily refresh read it.
    pub fn exchange(&self, code: &str, store: &Store, http: &Http) -> Result<String, String> {
        let q = query(&[
            ("client_id", &self.app_id),
            ("client_secret", &self.app_secret),
            ("grant_type", "authorization_code"),
            ("redirect_uri", &self.redirect_uri),
            ("code", code),
        ]);
        let short = http("oauth", "POST", &format!("https://graph.threads.net/oauth/access_token?{q}"), None, None)?;
        let short = short["access_token"].as_str().ok_or("oauth: code exchange returned no access_token")?;
        let q = query(&[("grant_type", "th_exchange_token"), ("client_secret", &self.app_secret), ("access_token", short)]);
        let long = http("oauth", "GET", &format!("https://graph.threads.net/access_token?{q}"), None, None)?;
        let (Some(token), Some(expires_in)) = (long["access_token"].as_str(), long["expires_in"].as_u64()) else {
            return Err("oauth: long-lived exchange returned no access_token or expires_in".into());
        };
        let me = http("oauth", "GET", &format!("{API}/me?{}", query(&[("fields", "id,username"), ("access_token", token)])), None, None)?;
        let username = me["username"].as_str().unwrap_or("");
        if !self.username.is_empty() && !username.eq_ignore_ascii_case(&self.username) {
            return Err(format!("token is for @{username}, expected @{}; not stored", self.username));
        }
        let expires_at = now() + expires_in;
        store.put("token", token);
        store.put("expires_at", &expires_at.to_string());
        metrics().token_expiry = expires_at;
        log(json!({"level": "info", "msg": "token registered", "username": username, "expires_at": expires_at}));
        Ok(format!("Token stored for @{username}, valid until {expires_at} (Unix time). From now on it is refreshed daily.\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_checks_account() {
        let fake = |username: &'static str| -> Http {
            Box::new(move |_, _, url, _, _| {
                Ok(if url.contains("/oauth/access_token") {
                    json!({"access_token": "short", "user_id": 1})
                } else if url.contains("th_exchange_token") {
                    json!({"access_token": "long", "expires_in": 5_184_000})
                } else {
                    json!({"id": "1", "username": username})
                })
            })
        };
        let o = OAuth { app_id: "1".into(), app_secret: "s".into(), redirect_uri: "https://x/cb".into(), username: "franz".into() };
        let store = Store::open(":memory:");
        assert!(o.exchange("c", &store, &fake("someone_else")).is_err());
        assert_eq!(store.get("token"), None);
        assert!(o.exchange("c", &store, &fake("Franz")).unwrap().contains("@Franz"));
        assert_eq!(store.get("token").as_deref(), Some("long"));
        assert!(store.get("expires_at").unwrap().parse::<u64>().unwrap() > now());
    }
}
