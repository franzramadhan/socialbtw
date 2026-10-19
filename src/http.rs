//! Outbound HTTP. A plain function type, so tests can swap in a fake.

use crate::metrics::metrics;
use crate::util::parsed;
use serde_json::Value;
use std::time::Duration;

/// Marker in errors from rate-limited calls; see `is_rate_limited`.
const RATE_LIMITED: &str = "rate limited";

/// Meta error codes for rate limits: app (4), user (17), page (32), API
/// call limit (613).
const META_RATE_LIMIT_CODES: [i64; 4] = [4, 17, 32, 613];

pub fn is_rate_limited(err: &str) -> bool {
    err.contains(RATE_LIMITED)
}

/// A 4xx answer that is not a rate limit: retrying the same call will not help.
pub fn is_client_error(err: &str) -> bool {
    !is_rate_limited(err) && err.contains(": HTTP 4")
}

/// Error text for an HTTP error status, marked when it is a rate limit.
fn status_error(endpoint: &str, code: u16, body: &str) -> String {
    let meta_code = serde_json::from_str::<Value>(body).ok().and_then(|v| v["error"]["code"].as_i64());
    let text: String = body.chars().take(500).collect();
    if code == 429 || meta_code.is_some_and(|c| META_RATE_LIMIT_CODES.contains(&c)) {
        format!("{endpoint}: {RATE_LIMITED} (HTTP {code}): {text}")
    } else {
        format!("{endpoint}: HTTP {code}: {text}")
    }
}

/// (endpoint label, method, url, bearer token, JSON body) -> JSON response.
/// The label names the call in errors and in threads_api_errors_total.
pub type Http = Box<dyn Fn(&'static str, &str, &str, Option<&str>, Option<&Value>) -> Result<Value, String>>;

pub fn real_http() -> Http {
    // Reasoning models can take 30s to over 2 minutes per answer.
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(parsed("HTTP_TIMEOUT_SECONDS", "120"))).build();
    Box::new(move |endpoint, method, url, bearer, body| {
        let mut req = agent.request(method, url).set("Content-Type", "application/json");
        if let Some(t) = bearer {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        // Never format the URL into errors: Threads URLs carry the access token.
        let out = match res {
            Ok(r) => r.into_json::<Value>().map_err(|e| format!("{endpoint}: bad JSON: {e}")),
            Err(ureq::Error::Status(code, r)) => Err(status_error(endpoint, code, &r.into_string().unwrap_or_default())),
            Err(ureq::Error::Transport(t)) => {
                // The message or the underlying error ("timed out"), but not
                // Display, which would include the URL.
                let detail = t.message().map(String::from).or_else(|| std::error::Error::source(&t).map(|s| s.to_string()));
                Err(format!("{endpoint}: {}: {}", t.kind(), detail.unwrap_or_default()))
            }
        };
        if out.is_err() {
            *metrics().api_errors.entry(endpoint).or_default() += 1;
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn rate_limits_are_recognized() {
        let meta = |c: i64| format!("{{\"error\":{{\"message\":\"limit\",\"code\":{c}}}}}");
        assert!(is_rate_limited(&status_error("posts", 429, "")));
        assert!(is_rate_limited(&status_error("posts", 400, &meta(4))));
        assert!(is_rate_limited(&status_error("hide", 403, &meta(613))));
        let not_found = status_error("hide", 400, &meta(100));
        assert!(!is_rate_limited(&not_found) && is_client_error(&not_found));
        let server = status_error("hide", 500, "oops");
        assert!(!is_rate_limited(&server) && !is_client_error(&server));
    }

    #[test]
    fn timeout_error_is_readable_and_hides_the_url() {
        // Accepts the connection, never answers.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/x?access_token=SECRET", listener.local_addr().unwrap());
        std::env::set_var("HTTP_TIMEOUT_SECONDS", "1");
        let err = real_http()("probe", "GET", &url, None, None).unwrap_err();
        drop(listener);
        assert!(err.starts_with("probe: "), "{err}");
        assert!(err.contains("timed out"), "{err}");
        assert!(!err.contains("SECRET"), "{err}");
    }
}
