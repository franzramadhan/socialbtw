//! Inbound HTTP: /metrics, and /oauth/start + /oauth/callback when OAuth is set.

use crate::http::real_http;
use crate::metrics::metrics;
use crate::oauth::OAuth;
use crate::store::Store;
use crate::util::{fatal, log, now, parse_query, random_hex};
use serde_json::json;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

/// (status line, extra headers, body)
type Response = (&'static str, String, String);

// ponytail: one-connection-at-a-time HTTP server, enough for a scraper every 30s
// and a human registering a token now and then.
pub fn serve(port: u16, oauth: Option<OAuth>, db_spec: String) {
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| fatal(&format!("port {port}: {e}")));
    thread::spawn(move || {
        // Own database connection and HTTP client: neither is shared with the poller thread.
        let store = oauth.as_ref().map(|_| Store::open(&db_spec));
        let http = real_http();
        // OAuth `state` values handed out by /oauth/start, with their expiry.
        let mut states: HashMap<String, u64> = HashMap::new();
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let target = req.split_whitespace().nth(1).unwrap_or("/");
            let (path, q) = target.split_once('?').unwrap_or((target, ""));
            let text = |status, body: String| -> Response { (status, "Content-Type: text/plain; charset=utf-8\r\n".into(), body) };
            let (status, headers, body): Response = match (path, &oauth, &store) {
                ("/metrics", _, _) => ("200 OK", "Content-Type: text/plain; version=0.0.4\r\n".into(), metrics().render()),
                ("/oauth/start", Some(o), _) => {
                    states.retain(|_, exp| *exp > now());
                    let state = random_hex(16);
                    let url = o.authorize_url(&state);
                    states.insert(state, now() + 600);
                    ("302 Found", format!("Location: {url}\r\n"), String::new())
                }
                ("/oauth/callback", Some(o), Some(store)) => {
                    let p = parse_query(q);
                    let state_ok = p.get("state").and_then(|s| states.remove(s)).is_some_and(|exp| exp > now());
                    match (state_ok, p.get("code"), p.get("error_description").or(p.get("error"))) {
                        (false, _, _) => text("400 Bad Request", "Unknown or expired state. Start again at /oauth/start.\n".into()),
                        (_, _, Some(e)) => text("400 Bad Request", format!("Threads refused: {e}\n")),
                        (_, None, _) => text("400 Bad Request", "No code in the callback.\n".into()),
                        (_, Some(code), _) => match o.exchange(code.trim_end_matches("#_"), store, &http) {
                            Ok(msg) => text("200 OK", msg),
                            Err(e) => {
                                log(json!({"level": "error", "msg": "token registration failed", "error": e}));
                                text("502 Bad Gateway", format!("{e}\n"))
                            }
                        },
                    }
                }
                _ => text("404 Not Found", "not found\n".into()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}
