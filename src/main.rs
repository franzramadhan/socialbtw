//! Hide unwanted replies on your own Threads posts.
//!
//! Polls your recent posts, classifies new top-level replies with Cloudflare
//! Clef, optionally asks an OpenAI-compatible LLM about uncertain ones, and
//! hides the bad ones through the Threads API. Decisions go to stdout as JSON
//! lines and to Prometheus metrics.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

const THREADS_API: &str = "https://graph.threads.net/v1.0";
const BAD: [&str; 4] = ["hateful", "harassment", "spam", "sexual"];

const CRITERIA: [(&str, &str); 6] = [
    ("hateful", "attacks a group or identity, slurs, dehumanizing language"),
    ("harassment", "personal attacks, insults, threats or mockery aimed at the post author or another person"),
    ("spam", "promotion, scams, crypto, links to unrelated sites, follow-for-follow, judi online or slot gambling promotion"),
    ("sexual", "unsolicited sexual content or sexual advances"),
    ("neutral", "on-topic reply, questions, disagreement or criticism without abuse"),
    ("positive", "supportive, thoughtful or adds value to the conversation"),
];

const LLM_PROMPT: &str = "You moderate replies on the author's own Threads posts.
Replies may be in Indonesian, English, or a mix, and may use slang.
Hide replies that are hateful, harassment, spam (including judi online or slot
promotion), or unsolicited sexual content. Keep criticism, disagreement, and
friendly jokes. {extra}
Answer with JSON only: {\"action\": \"hide\" | \"keep\" | \"review\", \"reason\": \"<one short sentence>\"}.
Use \"review\" only when you truly cannot decide.";

// ---- metrics ----------------------------------------------------------------

const BUCKETS: [f64; 12] = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95, 1.0];

struct Metrics {
    replies: BTreeMap<(String, String, String), u64>,
    bad_buckets: [u64; 12], // cumulative, Prometheus style
    bad_count: u64,
    bad_sum: f64,
    api_errors: BTreeMap<&'static str, u64>,
    last_poll: u64,
    token_expiry: u64,
}

static METRICS: Mutex<Metrics> = Mutex::new(Metrics {
    replies: BTreeMap::new(),
    bad_buckets: [0; 12],
    bad_count: 0,
    bad_sum: 0.0,
    api_errors: BTreeMap::new(),
    last_poll: 0,
    token_expiry: 0,
});

fn metrics() -> std::sync::MutexGuard<'static, Metrics> {
    METRICS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Metrics {
    fn observe_bad(&mut self, p: f64) {
        for (i, le) in BUCKETS.iter().enumerate() {
            if p <= *le {
                self.bad_buckets[i] += 1;
            }
        }
        self.bad_count += 1;
        self.bad_sum += p;
    }

    fn render(&self) -> String {
        let mut s = String::from("# TYPE threads_replies_total counter\n");
        for ((c, a, st), v) in &self.replies {
            s += &format!("threads_replies_total{{category=\"{c}\",action=\"{a}\",stage=\"{st}\"}} {v}\n");
        }
        s += "# TYPE threads_clef_bad_probability histogram\n";
        for (le, n) in BUCKETS.iter().zip(self.bad_buckets) {
            s += &format!("threads_clef_bad_probability_bucket{{le=\"{le}\"}} {n}\n");
        }
        s += &format!(
            "threads_clef_bad_probability_bucket{{le=\"+Inf\"}} {c}\nthreads_clef_bad_probability_sum {}\nthreads_clef_bad_probability_count {c}\n",
            self.bad_sum,
            c = self.bad_count
        );
        s += "# TYPE threads_api_errors_total counter\n";
        for (e, v) in &self.api_errors {
            s += &format!("threads_api_errors_total{{endpoint=\"{e}\"}} {v}\n");
        }
        s += &format!(
            "# TYPE threads_last_poll_success_timestamp_seconds gauge\nthreads_last_poll_success_timestamp_seconds {}\n\
             # TYPE threads_token_expiry_timestamp_seconds gauge\nthreads_token_expiry_timestamp_seconds {}\n",
            self.last_poll, self.token_expiry
        );
        s
    }
}

// ponytail: one-connection-at-a-time HTTP server, enough for a scraper every 30s.
fn serve_metrics(port: u16) {
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| fatal(&format!("metrics port {port}: {e}")));
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.read(&mut [0u8; 1024]);
            let body = metrics().render();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

// ---- helpers ----------------------------------------------------------------

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn log(mut fields: Value) {
    fields["ts"] = json!(now());
    println!("{fields}");
}

fn fatal(msg: &str) -> ! {
    log(json!({"level": "fatal", "msg": msg}));
    process::exit(1)
}

fn var(name: &str, default: Option<&str>) -> String {
    env::var(name).ok().or(default.map(String::from)).unwrap_or_else(|| fatal(&format!("missing required env {name}")))
}

fn parsed<T: std::str::FromStr>(name: &str, default: &str) -> T {
    var(name, Some(default)).parse().unwrap_or_else(|_| fatal(&format!("invalid {name}")))
}

fn csv_set(name: &str) -> HashSet<String> {
    var(name, Some(""))
        .split(',')
        .map(|s| s.trim().trim_start_matches(['#', '@']).to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn query(params: &[(&str, &str)]) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    params.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&")
}

/// (endpoint label, method, url, bearer token, JSON body) -> JSON response.
type Http = Box<dyn Fn(&'static str, &str, &str, Option<&str>, Option<&Value>) -> Result<Value, String>>;

fn real_http() -> Http {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(60)).build();
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
            Err(ureq::Error::Transport(t)) => Err(format!("{endpoint}: {}: {}", t.kind(), t.message().unwrap_or(""))),
        };
        if out.is_err() {
            *metrics().api_errors.entry(endpoint).or_default() += 1;
        }
        out
    })
}

// ---- decisions --------------------------------------------------------------

/// True when no filter is set, or the post's topic is in `topics`, or a #hashtag
/// in its text is in `hashtags`.
fn matches_filter(post: &Value, topics: &HashSet<String>, hashtags: &HashSet<String>) -> bool {
    if topics.is_empty() && hashtags.is_empty() {
        return true;
    }
    let topic = post["topic_tag"].as_str().map(|t| t.trim().trim_start_matches('#').to_lowercase());
    if topic.is_some_and(|t| topics.contains(&t)) {
        return true;
    }
    let text = post["text"].as_str().unwrap_or("");
    text.split('#')
        .skip(1)
        .map(|s| s.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>().to_lowercase())
        .any(|t| hashtags.contains(&t))
}

/// Map Clef category probabilities to (category, bad probability, action).
///
/// Bad probability is summed across bad categories, so a reply split between
/// "hateful" and "harassment" still counts as clearly bad.
fn decide(p: &BTreeMap<String, f64>, hide: f64, review: f64) -> (String, f64, &'static str) {
    let get = |c: &str| p.get(c).copied().unwrap_or(0.0);
    let bad_p: f64 = BAD.iter().map(|c| get(c)).sum();
    let worst = BAD.iter().fold(BAD[0], |best, &c| if get(c) > get(best) { c } else { best });
    if bad_p >= hide {
        return (worst.into(), bad_p, "hide");
    }
    if bad_p >= review {
        return (worst.into(), bad_p, "escalate");
    }
    let top = p.iter().fold(None, |best: Option<(&String, f64)>, (c, &v)| match best {
        Some((_, bv)) if bv >= v => best,
        _ => Some((c, v)),
    });
    (top.map(|(c, _)| c.clone()).unwrap_or_else(|| "neutral".into()), bad_p, "keep")
}

/// Pull {"action", "reason"} out of an LLM answer, tolerating code fences or prose.
fn parse_llm(content: &str) -> (String, String) {
    let parsed = content
        .find('{')
        .zip(content.rfind('}'))
        .filter(|(a, b)| a < b)
        .and_then(|(a, b)| serde_json::from_str::<Value>(&content[a..=b]).ok());
    match parsed {
        Some(v) => {
            let action = match v["action"].as_str() {
                Some(a @ ("hide" | "keep" | "review")) => a,
                _ => "review",
            };
            (action.into(), v["reason"].as_str().unwrap_or("").into())
        }
        None => ("review".into(), format!("unparseable LLM answer: {}", content.chars().take(200).collect::<String>())),
    }
}

// ---- state ------------------------------------------------------------------

/// SQLite: processed replies (so each is classified once) and the refreshed token.
struct Store(Connection);

impl Store {
    fn open(path: &str) -> Self {
        let db = Connection::open(path).unwrap_or_else(|e| fatal(&format!("open {path}: {e}")));
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS replies (
                 id TEXT PRIMARY KEY, post_id TEXT, username TEXT, text TEXT,
                 category TEXT, bad_probability REAL, action TEXT, stage TEXT,
                 reason TEXT, decided_at INTEGER);
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap_or_else(|e| fatal(&format!("init {path}: {e}")));
        Store(db)
    }

    fn seen(&self, id: &str) -> bool {
        self.0.query_row("SELECT 1 FROM replies WHERE id = ?", [id], |_| Ok(())).optional().ok().flatten().is_some()
    }

    fn get(&self, key: &str) -> Option<String> {
        self.0.query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0)).optional().ok().flatten()
    }

    fn put(&self, key: &str, value: &str) {
        if let Err(e) = self.0.execute("INSERT OR REPLACE INTO kv VALUES (?, ?)", [key, value]) {
            log(json!({"level": "error", "msg": "store write failed", "error": e.to_string()}));
        }
    }
}

// ---- monitor ----------------------------------------------------------------

struct Monitor {
    http: Http,
    store: Store,
    dry_run: bool,
    interval: u64,
    lookback: u64,
    hide_threshold: f64,
    review_threshold: f64,
    allowlist: HashSet<String>,
    topics: HashSet<String>,
    hashtags: HashSet<String>,
    extra_rules: String,
    clef_url: String,
    clef_token: String,
    clef_model: String,
    llm_url: String,
    llm_key: String,
    llm_model: String,
    next_refresh: u64,
}

impl Monitor {
    fn from_env(http: Http) -> Self {
        let m = Monitor {
            http,
            store: Store::open(&var("DB_PATH", Some("socialbtw.db"))),
            dry_run: var("DRY_RUN", Some("true")).to_lowercase() != "false",
            interval: parsed("POLL_INTERVAL_SECONDS", "300"),
            lookback: parsed::<u64>("LOOKBACK_DAYS", "3") * 86400,
            hide_threshold: parsed("HIDE_THRESHOLD", "0.85"),
            review_threshold: parsed("REVIEW_THRESHOLD", "0.5"),
            allowlist: csv_set("ALLOWLIST"),
            topics: csv_set("TOPICS"),
            hashtags: csv_set("HASHTAGS"),
            extra_rules: var("EXTRA_RULES", Some("")),
            clef_url: var("CLEF_URL", None),
            clef_token: var("CLEF_TOKEN", None),
            clef_model: var("CLEF_MODEL", Some("clef")),
            llm_url: var("LLM_BASE_URL", Some("")).trim_end_matches('/').into(),
            llm_key: var("LLM_API_KEY", Some("")),
            llm_model: var("LLM_MODEL", Some("")),
            next_refresh: 0,
        };
        // The token is refreshed daily and persisted, because it expires after 60 days.
        // Changing THREADS_ACCESS_TOKEN replaces the stored token.
        let seed = var("THREADS_ACCESS_TOKEN", None);
        if m.store.get("seed").as_deref() != Some(&seed) {
            m.store.put("seed", &seed);
            m.store.put("token", &seed);
            m.store.put("expires_at", "0");
        }
        metrics().token_expiry = m.store.get("expires_at").and_then(|v| v.parse().ok()).unwrap_or(0);
        m
    }

    fn token(&self) -> String {
        self.store.get("token").unwrap_or_default()
    }

    fn maybe_refresh(&mut self) {
        if now() < self.next_refresh {
            return;
        }
        self.next_refresh = now() + 86400;
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

    fn threads(&self, method: &str, path: &str, endpoint: &'static str, params: &[(&str, &str)]) -> Result<Value, String> {
        let token = self.token();
        let mut all = params.to_vec();
        all.push(("access_token", &token));
        (self.http)(endpoint, method, &format!("{THREADS_API}/{path}?{}", query(&all)), None, None)
    }

    fn paged(&self, path: &str, endpoint: &'static str, params: &[(&str, &str)]) -> Result<Vec<Value>, String> {
        let mut page = self.threads("GET", path, endpoint, params)?;
        let mut out = Vec::new();
        loop {
            out.extend(page["data"].as_array().cloned().unwrap_or_default());
            match page["paging"]["next"].as_str().map(String::from) {
                Some(next) => page = (self.http)(endpoint, "GET", &next, None, None)?,
                None => return Ok(out),
            }
        }
    }

    fn clef(&self, post: &str, reply: &str, author: &str) -> Result<BTreeMap<String, f64>, String> {
        let criteria: Map<String, Value> = CRITERIA.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let body = json!({
            "model": self.clef_model,
            "state": {"post": post, "reply": reply, "reply_author": author},
            "questions": {"category": {
                "type": "choice",
                "instructions": "Classify the reply in the context of the post. \
                                 The reply may be in Indonesian, English, or mixed, and may use slang.",
                "criteria": criteria,
            }},
        });
        let resp = (self.http)("clef", "POST", &self.clef_url, Some(&self.clef_token), Some(&body))?;
        // Cloudflare wraps results in {"result": ...}; a pass-through proxy may not.
        let result = resp.get("result").unwrap_or(&resp);
        result["answers"]["category"]["probabilities"]
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0))).collect())
            .ok_or_else(|| format!("clef: unexpected response: {}", result.to_string().chars().take(300).collect::<String>()))
    }

    fn llm(&self, post: &str, reply: &str, author: &str, p: &BTreeMap<String, f64>) -> Result<(String, String), String> {
        let user = json!({"post": post, "reply": reply, "reply_author": author, "classifier_probabilities": p});
        let body = json!({
            "model": self.llm_model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": LLM_PROMPT.replace("{extra}", &self.extra_rules)},
                {"role": "user", "content": user.to_string()},
            ],
        });
        let resp = (self.http)("llm", "POST", &format!("{}/chat/completions", self.llm_url), Some(&self.llm_key), Some(&body))?;
        Ok(parse_llm(resp["choices"][0]["message"]["content"].as_str().unwrap_or("")))
    }

    fn handle(&self, post: &Value, reply: &Value) -> Result<(), String> {
        let id = reply["id"].as_str().ok_or("reply without id")?;
        let text = reply["text"].as_str().unwrap_or("");
        let username = reply["username"].as_str().unwrap_or("");
        let post_text = post["text"].as_str().unwrap_or("");

        let probs = self.clef(post_text, text, username)?;
        let (category, bad_p, first) = decide(&probs, self.hide_threshold, self.review_threshold);
        metrics().observe_bad(bad_p);
        let (mut stage, mut reason, mut action) = ("clef", String::new(), first.to_string());
        if action == "escalate" {
            if !self.llm_url.is_empty() && !self.llm_model.is_empty() {
                stage = "llm";
                (action, reason) = self.llm(post_text, text, username, &probs)?;
            } else {
                action = "review".into();
            }
        }
        let action = match action.as_str() {
            "hide" if self.dry_run => "would_hide",
            "hide" => {
                self.threads("POST", &format!("{id}/manage_reply"), "hide", &[("hide", "true")])?;
                "hidden"
            }
            "keep" => "kept",
            _ => "review",
        };

        let post_id = post["id"].as_str().unwrap_or("");
        self.store
            .0
            .execute(
                "INSERT OR REPLACE INTO replies VALUES (?,?,?,?,?,?,?,?,?,?)",
                params![id, post_id, username, text, category, bad_p, action, stage, reason, now() as i64],
            )
            .map_err(|e| format!("store: {e}"))?;
        *metrics().replies.entry((category.clone(), action.into(), stage.into())).or_default() += 1;
        log(json!({
            "level": "info", "msg": "decision", "dry_run": self.dry_run,
            "reply_id": id, "post_id": post_id, "username": username, "text": text,
            "category": category, "bad_probability": (bad_p * 10000.0).round() / 10000.0,
            "probabilities": probs, "action": action, "stage": stage, "reason": reason,
            "permalink": reply["permalink"].as_str().unwrap_or(""),
        }));
        Ok(())
    }

    fn poll(&self) -> Result<(), String> {
        let since = now().saturating_sub(self.lookback).to_string();
        let posts = self.paged("me/threads", "posts", &[("fields", "id,text,timestamp,topic_tag,media_type"), ("since", &since)])?;
        // Reposts show up in your own feed, but their replies belong to the
        // original author's thread and cannot be hidden by you.
        let own = posts.iter().filter(|p| p["media_type"].as_str() != Some("REPOST_FACADE"));
        for post in own.filter(|p| matches_filter(p, &self.topics, &self.hashtags)) {
            let Some(post_id) = post["id"].as_str() else { continue };
            let replies = self.paged(
                &format!("{post_id}/replies"),
                "replies",
                &[("fields", "id,text,username,permalink,timestamp,hide_status,is_reply_owned_by_me")],
            )?;
            for reply in &replies {
                // Skip your own replies, allowlisted users, image-only replies, replies already
                // handled, and anything not in its default state: a reply you unhid by hand
                // (UNHUSHED) is never hidden again.
                let username = reply["username"].as_str().unwrap_or("").to_lowercase();
                if reply["is_reply_owned_by_me"].as_bool() == Some(true)
                    || reply["text"].as_str().map_or(true, str::is_empty)
                    || self.allowlist.contains(&username)
                    || reply["hide_status"].as_str().unwrap_or("NOT_HUSHED") != "NOT_HUSHED"
                    || reply["id"].as_str().map_or(true, |id| self.store.seen(id))
                {
                    continue;
                }
                // One bad reply must not block the rest; it is retried next poll.
                if let Err(e) = self.handle(post, reply) {
                    log(json!({"level": "error", "msg": "reply failed", "reply_id": reply["id"], "error": e}));
                }
            }
        }
        Ok(())
    }

    fn run(mut self) {
        let sorted = |s: &HashSet<String>| {
            let mut v: Vec<_> = s.iter().cloned().collect();
            v.sort();
            v
        };
        log(json!({
            "level": "info", "msg": "starting", "dry_run": self.dry_run, "interval": self.interval,
            "topics": sorted(&self.topics), "hashtags": sorted(&self.hashtags), "llm": !self.llm_url.is_empty() && !self.llm_model.is_empty(),
        }));
        loop {
            self.maybe_refresh();
            match self.poll() {
                Ok(()) => metrics().last_poll = now(),
                Err(e) => log(json!({"level": "error", "msg": "poll failed", "error": e})),
            }
            thread::sleep(Duration::from_secs(self.interval));
        }
    }
}

extern "C" fn on_term(_: libc::c_int) {
    // As PID 1 in a container the default SIGTERM action is ignored; exit right away.
    // Each SQLite write is its own transaction, so nothing is half-written.
    unsafe { libc::_exit(0) }
}

fn main() {
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    serve_metrics(parsed("METRICS_PORT", "9090"));
    Monitor::from_env(real_http()).run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn probs(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn decide_thresholds() {
        assert_eq!(decide(&probs(&[("spam", 0.95), ("neutral", 0.05)]), 0.85, 0.5), ("spam".into(), 0.95, "hide"));
        // Split across bad categories still sums to a hide.
        let (cat, p, action) = decide(&probs(&[("hateful", 0.45), ("harassment", 0.44), ("neutral", 0.11)]), 0.85, 0.5);
        assert_eq!((cat.as_str(), action), ("hateful", "hide"));
        assert!((p - 0.89).abs() < 1e-9);
        assert_eq!(decide(&probs(&[("harassment", 0.6), ("neutral", 0.4)]), 0.85, 0.5).2, "escalate");
        assert_eq!(decide(&probs(&[("positive", 0.9), ("spam", 0.1)]), 0.85, 0.5), ("positive".into(), 0.1, "keep"));
    }

    #[test]
    fn post_filter() {
        let set = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        let none = set(&[]);
        assert!(matches_filter(&json!({"text": "hi"}), &none, &none));
        assert!(matches_filter(&json!({"text": "hi", "topic_tag": "Home Lab"}), &set(&["home lab"]), &none));
        assert!(matches_filter(&json!({"text": "new post #K8s today"}), &none, &set(&["k8s", "golang"])));
        // A topic filter does not match a hashtag with the same word, and the other way round.
        assert!(!matches_filter(&json!({"text": "#homelab"}), &set(&["homelab"]), &none));
        assert!(!matches_filter(&json!({"text": "hi", "topic_tag": "homelab"}), &none, &set(&["homelab"])));
        assert!(!matches_filter(&json!({"text": "no tags", "topic_tag": "cooking"}), &set(&["homelab"]), &none));
    }

    #[test]
    fn llm_answer_parsing() {
        assert_eq!(parse_llm("```json\n{\"action\": \"hide\", \"reason\": \"promo\"}\n```"), ("hide".into(), "promo".into()));
        assert_eq!(parse_llm("{\"action\": \"delete\"}").0, "review");
        assert_eq!(parse_llm("no idea").0, "review");
    }

    // Runs every handle() case in one test: they share process env vars.
    #[test]
    fn handle_actions() {
        let run = |dry_run: bool, clef: Value, llm: Option<&str>| -> (String, Vec<&'static str>) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (c, clef, llm_answer) = (calls.clone(), clef, llm.map(String::from));
            let http: Http = Box::new(move |endpoint, _, _, _, _| {
                c.borrow_mut().push(endpoint);
                Ok(match endpoint {
                    "clef" => json!({"result": {"answers": {"category": {"probabilities": clef}}}}),
                    "llm" => json!({"choices": [{"message": {"content": llm_answer.clone().unwrap_or_default()}}]}),
                    _ => json!({"success": true}),
                })
            });
            for (k, v) in [
                ("THREADS_ACCESS_TOKEN", "t"),
                ("CLEF_URL", "http://clef"),
                ("CLEF_TOKEN", "c"),
                ("DB_PATH", ":memory:"),
                ("DRY_RUN", if dry_run { "true" } else { "false" }),
                ("LLM_BASE_URL", if llm.is_some() { "http://llm" } else { "" }),
                ("LLM_MODEL", if llm.is_some() { "m" } else { "" }),
            ] {
                env::set_var(k, v);
            }
            let m = Monitor::from_env(http);
            m.handle(&json!({"id": "p1", "text": "post"}), &json!({"id": "r1", "text": "slot gacor", "username": "x"})).unwrap();
            let action: String = m.store.0.query_row("SELECT action FROM replies WHERE id = 'r1'", [], |r| r.get(0)).unwrap();
            let calls = calls.borrow().clone();
            (action, calls)
        };
        let gray = json!({"spam": 0.6, "neutral": 0.4});
        assert_eq!(run(true, json!({"spam": 0.99}), None), ("would_hide".into(), vec!["clef"]));
        assert_eq!(run(false, json!({"spam": 0.99}), None), ("hidden".into(), vec!["clef", "hide"]));
        assert_eq!(run(false, gray.clone(), None), ("review".into(), vec!["clef"]));
        assert_eq!(
            run(false, gray.clone(), Some("{\"action\": \"hide\", \"reason\": \"gambling promo\"}")),
            ("hidden".into(), vec!["clef", "llm", "hide"])
        );
        assert_eq!(run(false, gray, Some("no idea")), ("review".into(), vec!["clef", "llm"]));
    }
}
