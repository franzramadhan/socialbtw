//! Small shared helpers: time, JSON logging, env config, URL encoding.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One JSON object per line on stdout.
pub fn log(mut fields: Value) {
    fields["ts"] = json!(now());
    println!("{fields}");
}

pub fn fatal(msg: &str) -> ! {
    log(json!({"level": "fatal", "msg": msg}));
    process::exit(1)
}

/// Env var, or `default`; missing with no default is fatal.
pub fn var(name: &str, default: Option<&str>) -> String {
    env::var(name).ok().or(default.map(String::from)).unwrap_or_else(|| fatal(&format!("missing required env {name}")))
}

pub fn parsed<T: std::str::FromStr>(name: &str, default: &str) -> T {
    var(name, Some(default)).parse().unwrap_or_else(|_| fatal(&format!("invalid {name}")))
}

/// Comma-separated env var as a lowercase set, without leading '#' or '@'.
pub fn csv_set(name: &str) -> HashSet<String> {
    var(name, Some(""))
        .split(',')
        .map(|s| s.trim().trim_start_matches(['#', '@']).to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// "5m", "1h", "30d" -> seconds.
pub fn duration(s: &str) -> Option<u64> {
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

/// URL query string with every value percent-encoded.
pub fn query(params: &[(&str, &str)]) -> String {
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

pub fn decode(s: &str) -> String {
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

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect()
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .unwrap_or_else(|e| fatal(&format!("/dev/urandom: {e}")));
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The first {...} object in a model answer, tolerating code fences or prose around it.
pub fn extract_json(content: &str) -> Option<Value> {
    let (a, b) = content.find('{').zip(content.rfind('}')).filter(|(a, b)| a < b)?;
    serde_json::from_str(&content[a..=b]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decoding() {
        let p = parse_query("code=AQB%2Fx-y_z&state=ab12&error_description=Permissions+error");
        assert_eq!(p["code"], "AQB/x-y_z");
        assert_eq!(p["state"], "ab12");
        assert_eq!(p["error_description"], "Permissions error");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(query(&[("a", "x y/z")]), "a=x%20y%2Fz");
        assert_eq!(random_hex(16).len(), 32);
    }

    #[test]
    fn durations_and_json() {
        assert_eq!(duration("5m"), Some(300));
        assert_eq!(duration("30d"), Some(2_592_000));
        assert_eq!(duration("5x"), None);
        assert_eq!(extract_json("```json\n{\"a\": 1}\n```"), Some(json!({"a": 1})));
        assert_eq!(extract_json("no json"), None);
    }
}
