//! The poll loop: which posts to read, which replies are new, and what to do
//! with each one.

use crate::classifier::{self, decide, Classifier, Input};
use crate::http::Http;
use crate::metrics::metrics;
use crate::reviewer::Reviewer;
use crate::store::{db_spec, Decision, Store};
use crate::threads::{parse_ts, Threads};
use crate::util::{csv_set, duration, fatal, log, now, parsed, var};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::thread;
use std::time::Duration;

pub struct Monitor {
    pub http: Http,
    pub store: Store,
    classifier: Box<dyn Classifier>,
    reviewer: Option<Reviewer>,
    dry_run: bool,
    interval: u64,
    lookback: u64,
    hide_threshold: f64,
    review_threshold: f64,
    allowlist: HashSet<String>,
    topics: HashSet<String>,
    hashtags: HashSet<String>,
    schedule: Vec<(u64, u64)>,
    next_refresh: u64,
    // ponytail: in memory; after a restart every post is checked once, then backs off again
    next_check: HashMap<String, u64>,
    last_activity: HashMap<String, u64>,
}

impl Monitor {
    pub fn from_env(http: Http) -> Self {
        let extra_rules = var("EXTRA_RULES", Some(""));
        let m = Monitor {
            http,
            store: Store::open(&db_spec()),
            classifier: classifier::from_env(&extra_rules),
            reviewer: Reviewer::from_env(&extra_rules),
            dry_run: var("DRY_RUN", Some("true")).to_lowercase() != "false",
            interval: parsed("POLL_INTERVAL_SECONDS", "300"),
            lookback: parsed::<u64>("LOOKBACK_DAYS", "3") * 86400,
            hide_threshold: parsed("HIDE_THRESHOLD", "0.85"),
            review_threshold: parsed("REVIEW_THRESHOLD", "0.5"),
            allowlist: csv_set("ALLOWLIST"),
            topics: csv_set("TOPICS"),
            hashtags: csv_set("HASHTAGS"),
            schedule: parse_schedule(&var("POST_CHECK_SCHEDULE", Some("1d:5m,7d:1h,30d:6h")))
                .unwrap_or_else(|| fatal("invalid POST_CHECK_SCHEDULE, expected like 1d:5m,7d:1h,30d:6h")),
            next_refresh: 0,
            next_check: HashMap::new(),
            last_activity: HashMap::new(),
        };
        m.threads().seed(&var("THREADS_ACCESS_TOKEN", Some("")));
        m
    }

    pub fn threads(&self) -> Threads<'_> {
        Threads { http: &self.http, store: &self.store }
    }

    fn handle(&self, post: &Value, reply: &Value) -> Result<(), String> {
        let id = reply["id"].as_str().ok_or("reply without id")?;
        let text = reply["text"].as_str().unwrap_or("");
        let username = reply["username"].as_str().unwrap_or("");
        let input = Input { post: post["text"].as_str().unwrap_or(""), reply: text, author: username };

        let probs = self.classifier.classify(&self.http, &input)?;
        let (category, bad_p, first) = decide(&probs, self.hide_threshold, self.review_threshold);
        metrics().observe_bad(bad_p);
        let (mut stage, mut reason, mut action) = ("classifier", String::new(), first.to_string());
        if action == "escalate" {
            match &self.reviewer {
                Some(r) => {
                    stage = "reviewer";
                    (action, reason) = r.review(&self.http, &input, &probs)?;
                }
                None => action = "review".into(),
            }
        }
        let action = match action.as_str() {
            "hide" if self.dry_run => "would_hide",
            "hide" => {
                self.threads().hide(id)?;
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
        let posts = self.threads().posts(now().saturating_sub(self.lookback))?;
        let ids: HashSet<&str> = posts.iter().filter_map(|p| p["id"].as_str()).collect();
        self.next_check.retain(|id, _| ids.contains(id.as_str()));
        self.last_activity.retain(|id, _| ids.contains(id.as_str()));
        // Reposts show up in your own feed, but their replies belong to the
        // original author's thread and cannot be hidden by you.
        let own = posts.iter().filter(|p| p["media_type"].as_str() != Some("REPOST_FACADE"));
        for post in own.filter(|p| matches_filter(p, &self.topics, &self.hashtags)) {
            let Some(post_id) = post["id"].as_str() else { continue };
            // Quiet old posts are checked less often (POST_CHECK_SCHEDULE).
            if now() < self.next_check.get(post_id).copied().unwrap_or(0) {
                *metrics().post_checks.entry("skipped").or_default() += 1;
                continue;
            }
            let replies = self.threads().replies(post_id)?;
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

    pub fn run(mut self) {
        let sorted = |s: &HashSet<String>| {
            let mut v: Vec<_> = s.iter().cloned().collect();
            v.sort();
            v
        };
        log(json!({
            "level": "info", "msg": "starting", "dry_run": self.dry_run, "interval": self.interval,
            "topics": sorted(&self.topics), "hashtags": sorted(&self.hashtags),
            "classifier": var("CLASSIFIER", Some("systemone")), "reviewer": self.reviewer.is_some(),
        }));
        loop {
            if self.threads().token().is_empty() {
                log(json!({"level": "warning", "msg": "no Threads token: set THREADS_ACCESS_TOKEN or open /oauth/start"}));
                thread::sleep(Duration::from_secs(self.interval));
                continue;
            }
            if now() >= self.next_refresh {
                self.next_refresh = now() + 86400;
                self.threads().refresh();
            }
            match self.poll() {
                Ok(()) => metrics().last_poll = now(),
                Err(e) => log(json!({"level": "error", "msg": "poll failed", "error": e})),
            }
            thread::sleep(Duration::from_secs(self.interval));
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::env;
    use std::rc::Rc;

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
    fn schedule() {
        let tiers = parse_schedule("7d:1h, 1d:5m,30d:6h").unwrap();
        assert_eq!(tiers, vec![(86400, 300), (604_800, 3600), (2_592_000, 21_600)]);
        assert_eq!(check_every(&tiers, 3600), 300);
        assert_eq!(check_every(&tiers, 2 * 86400), 3600);
        assert_eq!(check_every(&tiers, 20 * 86400), 21_600);
        assert_eq!(check_every(&tiers, 90 * 86400), 21_600);
        assert!(parse_schedule("1d:5x").is_none());
        assert!(parse_schedule("").is_none());
    }

    // Runs every handle() case in one test: they share process env vars.
    #[test]
    fn handle_actions() {
        let run = |dry_run: bool, probs: Value, verdict: Option<&str>| -> (String, Vec<&'static str>) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (c, verdict_text) = (calls.clone(), verdict.map(String::from));
            let http: Http = Box::new(move |endpoint, _, _, _, _| {
                c.borrow_mut().push(endpoint);
                Ok(match endpoint {
                    "classifier" => json!({"result": {"answers": {"category": {"probabilities": probs}}}}),
                    "reviewer" => json!({"choices": [{"message": {"content": verdict_text.clone().unwrap_or_default()}}]}),
                    _ => json!({"success": true}),
                })
            });
            for (k, v) in [
                ("THREADS_ACCESS_TOKEN", "t"),
                ("CLASSIFIER_URL", "http://classifier"),
                ("CLASSIFIER_TOKEN", "c"),
                ("DB_PATH", ":memory:"),
                ("DRY_RUN", if dry_run { "true" } else { "false" }),
                ("LLM_BASE_URL", if verdict.is_some() { "http://llm" } else { "" }),
                ("LLM_MODEL", if verdict.is_some() { "m" } else { "" }),
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
        assert_eq!(run(true, json!({"spam": 0.99}), None), ("would_hide".into(), vec!["classifier"]));
        assert_eq!(run(false, json!({"spam": 0.99}), None), ("hidden".into(), vec!["classifier", "hide"]));
        assert_eq!(run(false, gray.clone(), None), ("review".into(), vec!["classifier"]));
        assert_eq!(
            run(false, gray.clone(), Some("{\"action\": \"hide\", \"reason\": \"gambling promo\"}")),
            ("hidden".into(), vec!["classifier", "reviewer", "hide"])
        );
        assert_eq!(run(false, gray, Some("no idea")), ("review".into(), vec!["classifier", "reviewer"]));
    }
}
