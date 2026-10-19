//! Hide unwanted replies on your own Threads posts.
//!
//! Polls your recent posts, classifies new top-level replies with Cloudflare
//! Clef, optionally asks an OpenAI-compatible LLM about uncertain ones, and
//! hides the bad ones through the Threads API. Decisions go to stdout as JSON
//! lines and to Prometheus metrics.

use rusqlite::{params, Connection, OptionalExtension};
use rustls::pki_types::{pem::PemObject, CertificateDer};
use std::cell::RefCell;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, process, thread};

const THREADS_API: &str = "https://graph.threads.net/v1.0";
const BAD: [&str; 4] = ["hateful", "harassment", "spam", "sexual"];

const CRITERIA: [(&str, &str); 6] = [
    ("hateful", "attacks a group or identity, slurs, dehumanizing language"),
    ("harassment", "personal attacks, insults, threats or mockery aimed at the post author or another person"),
    ("spam", "advertising or self-promotion: recommending or naming a product, service, shop, seller or brand with a price, \
              discount or offer; links, domains or URLs (including obfuscated ones like '(dot)'), referral or affiliate codes and links, \
              'link in bio' or 'DM me' offers, scams, crypto, follow-for-follow, judi online or slot gambling promotion. \
              A reply can be friendly and on-topic and still be spam"),
    ("sexual", "unsolicited sexual content or sexual advances"),
    ("neutral", "on-topic reply, questions, disagreement or criticism without abuse and without promoting anything"),
    ("positive", "supportive, thoughtful or adds value to the conversation, without promoting a product, seller or link"),
];

const LLM_PROMPT: &str = "You moderate replies on the author's own Threads posts.
Replies may be in Indonesian, English, or a mix, and may use slang.
Hide replies that are hateful, harassment, spam, or unsolicited sexual content.
Spam includes product or service promotion (a shop, seller or brand with a price
or offer, even when friendly and on-topic), links or domains (also obfuscated,
like "(dot)"), referral or affiliate codes, and judi online or slot promotion.
Keep criticism, disagreement, and friendly jokes. {extra}
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
    post_checks: BTreeMap<&'static str, u64>,
    token_expiry: u64,
}

static METRICS: Mutex<Metrics> = Mutex::new(Metrics {
    replies: BTreeMap::new(),
    bad_buckets: [0; 12],
    bad_count: 0,
    bad_sum: 0.0,
    api_errors: BTreeMap::new(),
    last_poll: 0,
    post_checks: BTreeMap::new(),
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
        s += "# TYPE threads_post_checks_total counter\n";
        for (r, v) in &self.post_checks {
            s += &format!("threads_post_checks_total{{result=\"{r}\"}} {v}\n");
        }
        s += &format!(
            "# TYPE threads_last_poll_success_timestamp_seconds gauge\nthreads_last_poll_success_timestamp_seconds {}\n\
             # TYPE threads_token_expiry_timestamp_seconds gauge\nthreads_token_expiry_timestamp_seconds {}\n",
            self.last_poll, self.token_expiry
        );
        s
    }
}

// ---- HTTP server: /metrics, and /oauth/* when OAuth is configured -----------

/// Threads OAuth settings. With all of them set, /oauth/start and /oauth/callback
/// register a new token from the browser.
struct OAuth {
    app_id: String,
    app_secret: String,
    redirect_uri: String,
    /// Expected Threads account; a token for anyone else is rejected. Empty: any.
    username: String,
}

impl OAuth {
    fn from_env() -> Option<Self> {
        let get = |k: &str| var(k, Some(""));
        let (app_id, app_secret, redirect_uri) = (get("THREADS_APP_ID"), get("THREADS_APP_SECRET"), get("OAUTH_REDIRECT_URI"));
        if app_id.is_empty() || app_secret.is_empty() || redirect_uri.is_empty() {
            return None;
        }
        Some(OAuth { app_id, app_secret, redirect_uri, username: get("THREADS_USERNAME").trim_start_matches('@').into() })
    }

    fn authorize_url(&self, state: &str) -> String {
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
    fn exchange(&self, code: &str, store: &Store, http: &Http) -> Result<String, String> {
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
        let me = http("oauth", "GET", &format!("{THREADS_API}/me?{}", query(&[("fields", "id,username"), ("access_token", token)])), None, None)?;
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

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .unwrap_or_else(|e| fatal(&format!("/dev/urandom: {e}")));
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%' && i + 2 < b.len())
            .then(|| std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match (b[i], hex) {
            (_, Some(v)) => { out.push(v); i += 3; }
            (b'+', _) => { out.push(b' '); i += 1; }
            (c, _) => { out.push(c); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect()
}

/// (status line, extra headers, body)
type Response = (&'static str, String, String);

// ponytail: one-connection-at-a-time HTTP server, enough for a scraper every 30s
// and a human registering a token now and then.
fn serve(port: u16, oauth: Option<OAuth>, db_path: String) {
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| fatal(&format!("port {port}: {e}")));
    thread::spawn(move || {
        // Own connection and HTTP client: neither is shared with the poller thread.
        let store = oauth.as_ref().map(|_| Store::open(&db_path));
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

/// Threads timestamp ("2026-10-19T09:19:56+0000") to Unix seconds.
// ponytail: ignores the offset; Threads always answers in UTC (+0000).
fn parse_ts(s: &str) -> Option<u64> {
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

fn duration(s: &str) -> Option<u64> {
    let (n, unit) = s.split_at(s.len().checked_sub(1)?);
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    Some(n.parse::<u64>().ok()? * unit)
}

/// "1d:5m,7d:1h,30d:6h" -> [(max age, check interval)], sorted by age.
fn parse_schedule(spec: &str) -> Option<Vec<(u64, u64)>> {
    let mut tiers = spec
        .split(',')
        .map(|t| t.trim().split_once(':').and_then(|(a, i)| Some((duration(a)?, duration(i)?))))
        .collect::<Option<Vec<_>>>()?;
    tiers.sort();
    (!tiers.is_empty()).then_some(tiers)
}

/// How long to wait before checking a post again, from the age of its last
/// activity (the post or its newest reply). Older than every tier: the last one.
fn check_every(tiers: &[(u64, u64)], age: u64) -> u64 {
    tiers.iter().find(|(max_age, _)| age < *max_age).or(tiers.last()).map_or(0, |(_, every)| *every)
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

/// One decided reply, as stored.
struct Decision<'a> {
    id: &'a str,
    post_id: &'a str,
    username: &'a str,
    text: &'a str,
    category: &'a str,
    bad_p: f64,
    action: &'a str,
    stage: &'a str,
    reason: &'a str,
}

/// Processed replies (so each is classified once) and the current token.
/// SQLite file by default; PostgreSQL when DATABASE_URL is set, so the app keeps
/// no local state.
enum Store {
    Sqlite(Connection),
    Postgres { url: String, tls: tokio_postgres_rustls::MakeRustlsConnect, client: RefCell<Option<postgres::Client>> },
}

const PG_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS replies (
     id TEXT PRIMARY KEY, post_id TEXT, username TEXT, text TEXT,
     category TEXT, bad_probability DOUBLE PRECISION, action TEXT, stage TEXT,
     reason TEXT, decided_at BIGINT);
 CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);";

/// DATABASE_URL if set, else the SQLite file DB_PATH.
fn db_spec() -> String {
    let url = var("DATABASE_URL", Some(""));
    if url.is_empty() { var("DB_PATH", Some("socialbtw.db")) } else { url }
}

/// TLS for PostgreSQL: the CA in DATABASE_CA_FILE (PEM), else public roots.
fn pg_tls() -> tokio_postgres_rustls::MakeRustlsConnect {
    let mut roots = rustls::RootCertStore::empty();
    let ca = var("DATABASE_CA_FILE", Some(""));
    if ca.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    } else {
        for cert in CertificateDer::pem_file_iter(&ca).unwrap_or_else(|e| fatal(&format!("DATABASE_CA_FILE {ca}: {e}"))) {
            let cert = cert.unwrap_or_else(|e| fatal(&format!("DATABASE_CA_FILE {ca}: {e}")));
            roots.add(cert).unwrap_or_else(|e| fatal(&format!("DATABASE_CA_FILE {ca}: {e}")));
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap_or_else(|e| fatal(&format!("tls: {e}")))
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

/// postgres::Error's Display is often just "db error"; add the server message
/// or the underlying cause.
fn pg_err(e: &postgres::Error) -> String {
    match (e.as_db_error(), std::error::Error::source(e)) {
        (Some(db), _) => format!("{e}: {} ({})", db.message(), db.code().code()),
        (None, Some(src)) => format!("{e}: {src}"),
        (None, None) => e.to_string(),
    }
}

impl Store {
    fn open(spec: &str) -> Self {
        if spec.starts_with("postgres://") || spec.starts_with("postgresql://") {
            // Connects lazily (and again after a failover), so a database that is
            // still starting does not stop the app.
            return Store::Postgres { url: spec.into(), tls: pg_tls(), client: RefCell::new(None) };
        }
        let db = Connection::open(spec).unwrap_or_else(|e| fatal(&format!("open {spec}: {e}")));
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS replies (
                 id TEXT PRIMARY KEY, post_id TEXT, username TEXT, text TEXT,
                 category TEXT, bad_probability REAL, action TEXT, stage TEXT,
                 reason TEXT, decided_at INTEGER);
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap_or_else(|e| fatal(&format!("init {spec}: {e}")));
        // The poller and the OAuth callback each hold a connection.
        let _ = db.busy_timeout(Duration::from_secs(5));
        Store::Sqlite(db)
    }

    /// Run `f` on the PostgreSQL client, connecting (and creating the tables) first
    /// when there is no live connection, and retrying once after a dropped one.
    fn pg<T>(&self, f: impl Fn(&mut postgres::Client) -> Result<T, postgres::Error>) -> Result<T, String> {
        let Store::Postgres { url, tls, client } = self else { unreachable!() };
        let mut slot = client.borrow_mut();
        for attempt in 0..2 {
            if slot.as_ref().map_or(true, |c| c.is_closed()) {
                // The error never contains the URL, so the password stays out of logs.
                let mut c = postgres::Client::connect(url, tls.clone()).map_err(|e| format!("postgres connect: {}", pg_err(&e)))?;
                c.batch_execute(PG_SCHEMA).map_err(|e| format!("postgres schema: {}", pg_err(&e)))?;
                *slot = Some(c);
            }
            let c = slot.as_mut().expect("connected above");
            match f(c) {
                Ok(v) => return Ok(v),
                Err(_) if attempt == 0 && c.is_closed() => continue,
                Err(e) => return Err(format!("postgres: {}", pg_err(&e))),
            }
        }
        Err("postgres: connection lost".into())
    }

    fn seen(&self, id: &str) -> Result<bool, String> {
        match self {
            Store::Sqlite(db) => db
                .query_row("SELECT 1 FROM replies WHERE id = ?", [id], |_| Ok(()))
                .optional()
                .map(|r| r.is_some())
                .map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| c.query_opt("SELECT 1 FROM replies WHERE id = $1", &[&id]).map(|r| r.is_some())),
        }
    }

    fn get(&self, key: &str) -> Option<String> {
        let res = match self {
            Store::Sqlite(db) => db.query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0)).optional().map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| c.query_opt("SELECT value FROM kv WHERE key = $1", &[&key]).map(|r| r.map(|r| r.get(0)))),
        };
        res.unwrap_or_else(|e| {
            log(json!({"level": "error", "msg": "store read failed", "error": e}));
            None
        })
    }

    fn put(&self, key: &str, value: &str) {
        let res = match self {
            Store::Sqlite(db) => db.execute("INSERT OR REPLACE INTO kv VALUES (?, ?)", [key, value]).map(|_| ()).map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| {
                c.execute("INSERT INTO kv VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value", &[&key, &value])
                    .map(|_| ())
            }),
        };
        if let Err(e) = res {
            log(json!({"level": "error", "msg": "store write failed", "error": e}));
        }
    }

    fn save(&self, d: &Decision) -> Result<(), String> {
        let at = now() as i64;
        match self {
            Store::Sqlite(db) => db
                .execute(
                    "INSERT OR REPLACE INTO replies VALUES (?,?,?,?,?,?,?,?,?,?)",
                    params![d.id, d.post_id, d.username, d.text, d.category, d.bad_p, d.action, d.stage, d.reason, at],
                )
                .map(|_| ())
                .map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| {
                c.execute(
                    "INSERT INTO replies VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (id) DO NOTHING",
                    &[&d.id, &d.post_id, &d.username, &d.text, &d.category, &d.bad_p, &d.action, &d.stage, &d.reason, &at],
                )
                .map(|_| ())
            }),
        }
    }

    #[cfg(test)]
    fn action(&self, id: &str) -> Option<String> {
        match self {
            Store::Sqlite(db) => db.query_row("SELECT action FROM replies WHERE id = ?", [id], |r| r.get(0)).optional().ok().flatten(),
            Store::Postgres { .. } => self.pg(|c| c.query_opt("SELECT action FROM replies WHERE id = $1", &[&id]).map(|r| r.map(|r| r.get(0)))).ok().flatten(),
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
    schedule: Vec<(u64, u64)>,
    // ponytail: in memory; after a restart every post is checked once, then backs off again
    next_check: HashMap<String, u64>,
    last_activity: HashMap<String, u64>,
}

impl Monitor {
    fn from_env(http: Http) -> Self {
        let m = Monitor {
            http,
            store: Store::open(&db_spec()),
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
            schedule: parse_schedule(&var("POST_CHECK_SCHEDULE", Some("1d:5m,7d:1h,30d:6h")))
                .unwrap_or_else(|| fatal("invalid POST_CHECK_SCHEDULE, expected like 1d:5m,7d:1h,30d:6h")),
            next_check: HashMap::new(),
            last_activity: HashMap::new(),
        };
        // The token is refreshed daily and persisted, because it expires after 60 days.
        // Changing THREADS_ACCESS_TOKEN replaces the stored token. It is optional
        // when tokens are registered through /oauth/start instead.
        let seed = var("THREADS_ACCESS_TOKEN", Some(""));
        if !seed.is_empty() && m.store.get("seed").as_deref() != Some(&seed) {
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
        self.store.save(&Decision { id, post_id, username, text, category: &category, bad_p, action, stage, reason: &reason })?;
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

    fn poll(&mut self) -> Result<(), String> {
        let since = now().saturating_sub(self.lookback).to_string();
        let posts = self.paged("me/threads", "posts", &[("fields", "id,text,timestamp,topic_tag,media_type"), ("since", &since)])?;
        // Reposts show up in your own feed, but their replies belong to the
        // original author's thread and cannot be hidden by you.
        let own = posts.iter().filter(|p| p["media_type"].as_str() != Some("REPOST_FACADE"));
        let ids: HashSet<&str> = posts.iter().filter_map(|p| p["id"].as_str()).collect();
        self.next_check.retain(|id, _| ids.contains(id.as_str()));
        self.last_activity.retain(|id, _| ids.contains(id.as_str()));
        for post in own.filter(|p| matches_filter(p, &self.topics, &self.hashtags)) {
            let Some(post_id) = post["id"].as_str() else { continue };
            // Quiet old posts are checked less often (POST_CHECK_SCHEDULE).
            if now() < self.next_check.get(post_id).copied().unwrap_or(0) {
                *metrics().post_checks.entry("skipped").or_default() += 1;
                continue;
            }
            let replies = self.paged(
                &format!("{post_id}/replies"),
                "replies",
                &[("fields", "id,text,username,permalink,timestamp,hide_status,is_reply_owned_by_me")],
            )?;
            *metrics().post_checks.entry("checked").or_default() += 1;
            // A new reply makes an old post "active" again, so it is checked often again.
            let posted = post["timestamp"].as_str().and_then(parse_ts).unwrap_or_else(now);
            let active = replies
                .iter()
                .filter_map(|r| r["timestamp"].as_str().and_then(parse_ts))
                .chain([posted, self.last_activity.get(post_id).copied().unwrap_or(0)])
                .max()
                .unwrap_or(posted);
            self.last_activity.insert(post_id.into(), active);
            let mut failed = false;
            for reply in &replies {
                // Skip your own replies, allowlisted users, image-only replies, replies already
                // handled, and anything not in its default state: a reply you unhid by hand
                // (UNHUSHED) is never hidden again.
                let username = reply["username"].as_str().unwrap_or("").to_lowercase();
                let Some(reply_id) = reply["id"].as_str() else { continue };
                // A database error stops the poll: otherwise every reply would look new
                // and be paid for again.
                if reply["is_reply_owned_by_me"].as_bool() == Some(true)
                    || reply["text"].as_str().map_or(true, str::is_empty)
                    || self.allowlist.contains(&username)
                    || reply["hide_status"].as_str().unwrap_or("NOT_HUSHED") != "NOT_HUSHED"
                    || self.store.seen(reply_id)?
                {
                    continue;
                }
                // One bad reply must not block the rest; it is retried next poll.
                if let Err(e) = self.handle(post, reply) {
                    failed = true;
                    log(json!({"level": "error", "msg": "reply failed", "reply_id": reply["id"], "error": e}));
                }
            }
            // A failed reply keeps its post on every poll until it goes through.
            let wait = if failed { 0 } else { check_every(&self.schedule, now().saturating_sub(active)) };
            self.next_check.insert(post_id.into(), now() + wait);
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
            if self.token().is_empty() {
                log(json!({"level": "warning", "msg": "no Threads token: set THREADS_ACCESS_TOKEN or open /oauth/start"}));
                thread::sleep(Duration::from_secs(self.interval));
                continue;
            }
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
    let oauth = OAuth::from_env();
    let monitor = Monitor::from_env(real_http()); // creates the tables first
    if oauth.is_none() && monitor.token().is_empty() {
        fatal("set THREADS_ACCESS_TOKEN, or THREADS_APP_ID + THREADS_APP_SECRET + OAUTH_REDIRECT_URI");
    }
    log(json!({"level": "info", "msg": "http", "oauth": oauth.is_some()}));
    serve(parsed("METRICS_PORT", "9090"), oauth, db_spec());
    monitor.run();
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

    #[test]
    fn timestamps_and_schedule() {
        assert_eq!(parse_ts("2026-10-19T09:19:56+0000"), Some(1_792_401_596));
        assert_eq!(parse_ts("2024-02-29T23:59:59+0000"), Some(1_709_251_199));
        assert_eq!(parse_ts("garbage"), None);
        let tiers = parse_schedule("7d:1h, 1d:5m,30d:6h").unwrap();
        assert_eq!(tiers, vec![(86400, 300), (604_800, 3600), (2_592_000, 21_600)]);
        assert_eq!(check_every(&tiers, 3600), 300);
        assert_eq!(check_every(&tiers, 2 * 86400), 3600);
        assert_eq!(check_every(&tiers, 20 * 86400), 21_600);
        assert_eq!(check_every(&tiers, 90 * 86400), 21_600);
        assert!(parse_schedule("1d:5x").is_none());
        assert!(parse_schedule("").is_none());
    }

    #[test]
    fn query_decoding() {
        let p = parse_query("code=AQB%2Fx-y_z&state=ab12&error_description=Permissions+error");
        assert_eq!(p["code"], "AQB/x-y_z");
        assert_eq!(p["state"], "ab12");
        assert_eq!(p["error_description"], "Permissions error");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(random_hex(16).len(), 32);
    }

    #[test]
    fn oauth_exchange() {
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

    // Needs a PostgreSQL server: TEST_DATABASE_URL=postgres://... (and DATABASE_CA_FILE
    // for a private CA). Skipped otherwise.
    #[test]
    fn postgres_store() {
        let Ok(url) = env::var("TEST_DATABASE_URL") else { return };
        let store = Store::open(&url);
        store.put("k", "v1");
        store.put("k", "v2");
        assert_eq!(store.get("k").as_deref(), Some("v2"));
        let d = Decision { id: "pg-r1", post_id: "p", username: "u", text: "t", category: "spam", bad_p: 0.9, action: "hidden", stage: "clef", reason: "" };
        store.save(&d).unwrap();
        store.save(&d).unwrap(); // second save is a no-op
        assert!(store.seen("pg-r1").unwrap());
        assert!(!store.seen("pg-nope").unwrap());
        assert_eq!(store.action("pg-r1").as_deref(), Some("hidden"));
        // Drop the first connection server-side, as a failover would.
        Store::open(&url)
            .pg(|c| c.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND datname = current_database()", &[]))
            .unwrap();
        assert_eq!(store.get("k").as_deref(), Some("v2"));
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
            let action = m.store.action("r1").unwrap();
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
