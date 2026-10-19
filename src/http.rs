//! Outbound HTTP. A plain function type, so tests can swap in a fake.

use crate::metrics::metrics;
use crate::util::parsed;
use serde_json::Value;
use std::time::Duration;

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
            Err(ureq::Error::Status(code, r)) => {
                let text: String = r.into_string().unwrap_or_default().chars().take(500).collect();
                Err(format!("{endpoint}: HTTP {code}: {text}"))
            }
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
