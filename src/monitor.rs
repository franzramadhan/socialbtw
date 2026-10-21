//! The poll loop: which posts to read, which replies are new, and what to do
//! with each one.

use crate::classifier::{self, decide, Classifier, Input, Probabilities};
use crate::http::{is_client_error, is_rate_limited, Http};
use crate::metrics::metrics;
use crate::notify::Notifier;
use crate::reviewer::Reviewer;
use crate::store::{db_spec, Decision, Store};
use crate::threads::{parse_ts, Threads};
use crate::util::{csv_set, duration, fatal, log, now, parsed, var};
use regex_lite::Regex;
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
    hide_pattern: Option<Regex>,
    notifier: Option<Notifier>,
    /// category -> your answer to a reply that stays visible
    auto_reply: HashMap<String, String>,
    /// AUTO_REPLY_MODE=post: answers are posted; otherwise sent to you as drafts.
    auto_reply_post: bool,
    auto_reply_cooldown: u64,
    allowlist: HashSet<String>,
    topics: HashSet<String>,
    hashtags: HashSet<String>,
    schedule: Vec<(u64, u64)>,
    max_posts_per_poll: usize,
    strike_limit: u64,
    strike_window: u64,
    min_backoff: u64,
    max_backoff: u64,
    /// Current pause after rate limits (0: none); doubles while they continue.
    backoff: u64,
    paused_until: u64,
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
            hide_pattern: hide_pattern(&var("HIDE_PATTERN", Some(""))),
            notifier: Notifier::from_env(),
            auto_reply: parse_auto_reply(&var("AUTO_REPLY", Some("")))
                .unwrap_or_else(|| fatal("invalid AUTO_REPLY, expected like harassment=Please keep it civil.|sexual=...")),
            auto_reply_post: match var("AUTO_REPLY_MODE", Some("draft")).as_str() {
                "draft" => false,
                "post" => true,
                _ => fatal("invalid AUTO_REPLY_MODE, expected draft or post"),
            },
            auto_reply_cooldown: duration(&var("AUTO_REPLY_COOLDOWN", Some("1d")))
                .unwrap_or_else(|| fatal("invalid AUTO_REPLY_COOLDOWN, expected like 1d")),
            allowlist: csv_set("ALLOWLIST"),
            topics: csv_set("TOPICS"),
            hashtags: csv_set("HASHTAGS"),
            schedule: parse_schedule(&var("POST_CHECK_SCHEDULE", Some("1d:5m,7d:1h,30d:6h")))
                .unwrap_or_else(|| fatal("invalid POST_CHECK_SCHEDULE, expected like 1d:5m,7d:1h,30d:6h")),
            max_posts_per_poll: parsed("MAX_POSTS_PER_POLL", "50"),
            strike_limit: parsed("STRIKE_LIMIT", "3"),
            strike_window: duration(&var("STRIKE_WINDOW", Some("30d"))).unwrap_or_else(|| fatal("invalid STRIKE_WINDOW, expected like 30d")),
            min_backoff: duration(&var("RATE_LIMIT_BACKOFF", Some("15m"))).unwrap_or_else(|| fatal("invalid RATE_LIMIT_BACKOFF, expected like 15m")),
            max_backoff: duration(&var("RATE_LIMIT_MAX_BACKOFF", Some("6h"))).unwrap_or_else(|| fatal("invalid RATE_LIMIT_MAX_BACKOFF, expected like 6h")),
            backoff: 0,
            paused_until: 0,
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

        // A HIDE_PATTERN match, or a copy of a reply that was already hidden, is
        // hidden without asking the classifier.
        let shortcut = match self.hide_pattern.as_ref().and_then(|p| p.find(text)) {
            Some(m) => Some(("spam".to_string(), "pattern", format!("matched HIDE_PATTERN: {}", m.as_str()))),
            None => copy_key(text).and_then(|k| self.store.get(&k)).filter(|v| !v.is_empty()).map(|v| {
                let (category, original) = v.split_once(' ').unwrap_or((&v, ""));
                (category.to_string(), "copypasta", format!("same text as hidden reply {original}"))
            }),
        };
        let (category, bad_p, probs, stage, reason, action) = match shortcut {
            Some((category, stage, reason)) => (category, 1.0, Probabilities::new(), stage, reason, "hide".to_string()),
            None => {
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
                (category, bad_p, probs, stage, reason, action)
            }
        };
        // A failed hide is saved as hide_pending and retried on its own
        // (retry_pending_hides), so the reply is not classified and paid for again.
        let mut hide_error = None;
        let action = match action.as_str() {
            "hide" if self.dry_run => "would_hide",
            "hide" => match self.threads().hide(id) {
                Ok(()) => "hidden",
                Err(e) => {
                    hide_error = Some(e);
                    "hide_pending"
                }
            },
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
        // Only a reply that stays visible gets an answer: yours would be hidden with it.
        if let (Some(base), "review" | "kept") = (self.auto_reply.get(&category), action) {
            self.answer(&input, id, reply["permalink"].as_str().unwrap_or(""), base);
        }
        if action == "review" {
            let message = format!("{text}\n\n{category}, P(bad) {bad_p:.2}{}", if reason.is_empty() { String::new() } else { format!(": {reason}") });
            self.notify(&format!("Review reply by @{username}"), &message, reply["permalink"].as_str().unwrap_or(""));
        }
        if matches!(action, "hidden" | "would_hide" | "hide_pending") {
            if let (Some(k), "classifier" | "reviewer") = (copy_key(text), stage) {
                self.store.put(&k, &format!("{category} {id}"));
            }
            self.check_strikes(username, reply);
        }
        match hide_error {
            // Stops the poll, so the backoff starts.
            Some(e) if is_rate_limited(&e) => Err(e),
            Some(e) => {
                log(json!({"level": "warning", "msg": "hide failed, will retry", "reply_id": id, "error": e}));
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// AUTO_REPLY: answer as you, at most once per author per AUTO_REPLY_COOLDOWN.
    /// With a reviewer, the model adapts `base` to the reply; without one, or if
    /// its draft is unusable, `base` is used as is. Drafts go to NOTIFY_URL;
    /// AUTO_REPLY_MODE=post posts them. Runs after the decision is saved, so it
    /// is never sent twice; a failure is logged and not retried.
    fn answer(&self, input: &Input, reply_id: &str, permalink: &str, base: &str) {
        let key = format!("answered:{}", input.author.to_lowercase());
        let last: u64 = self.store.get(&key).and_then(|v| v.parse().ok()).unwrap_or(0);
        let (mut text, mut source, mut draft_error) = (base.to_string(), "static", None);
        let (result, error) = if now() < last + self.auto_reply_cooldown {
            ("cooldown", None)
        } else {
            match self.reviewer.as_ref().map(|r| r.draft(&self.http, input, base)) {
                Some(Ok(t)) => match self.check_draft(input, &t) {
                    Ok(()) => (text, source) = (t, "llm"),
                    Err(e) => draft_error = Some(e),
                },
                Some(Err(e)) => draft_error = Some(e),
                None => {}
            }
            match (self.auto_reply_post, self.dry_run) {
                (false, _) => {
                    let message = format!("{text}\n\nTheir reply: {}", input.reply);
                    self.notify(&format!("Answer @{}?", input.author), &message, permalink);
                    ("drafted", None)
                }
                (true, true) => ("would_reply", None),
                (true, false) => match self.threads().reply(reply_id, &text) {
                    Ok(()) => ("replied", None),
                    Err(e) => ("failed", Some(e)),
                },
            }
        };
        if matches!(result, "drafted" | "replied") {
            self.store.put(&key, &now().to_string());
        }
        log(json!({
            "level": if error.is_some() || draft_error.is_some() { "warning" } else { "info" }, "msg": "auto_reply",
            "reply_id": reply_id, "username": input.author, "answer": text, "source": source,
            "result": result, "error": error, "draft_error": draft_error,
        }));
    }

    /// Guards against prompt injection in the reply: a drafted answer must pass
    /// your own filters, HIDE_PATTERN and the classifier (not bad, not spam).
    fn check_draft(&self, input: &Input, draft: &str) -> Result<(), String> {
        if let Some(m) = self.hide_pattern.as_ref().and_then(|p| p.find(draft)) {
            return Err(format!("draft matches HIDE_PATTERN: {}", m.as_str()));
        }
        let probs = self.classifier.classify(&self.http, &Input { post: input.post, reply: draft, author: "" })?;
        let (category, bad_p, _) = decide(&probs, self.hide_threshold, self.review_threshold);
        if category == "spam" || bad_p >= self.review_threshold {
            return Err(format!("draft classified {category}, P(bad) {bad_p:.2}: {draft}"));
        }
        Ok(())
    }

    fn notify(&self, title: &str, message: &str, click: &str) {
        if let Some(n) = &self.notifier {
            n.send(&self.http, title, message, click);
        }
    }

    /// Threads has no block API: once an author has `strike_limit` hidden replies
    /// within `strike_window`, every further hide logs block_suggested, for you to block by hand.
    fn check_strikes(&self, username: &str, reply: &Value) {
        if self.strike_limit == 0 || username.is_empty() {
            return;
        }
        match self.store.strikes(username, now().saturating_sub(self.strike_window)) {
            Ok(n) if n >= self.strike_limit => {
                metrics().block_suggested += 1;
                log(json!({
                    "level": "warning", "msg": "block_suggested", "username": username, "strikes": n,
                    "window_days": self.strike_window / 86400, "profile": format!("https://www.threads.com/@{username}"),
                    "permalink": reply["permalink"].as_str().unwrap_or(""),
                }));
                let days = self.strike_window / 86400;
                let message = format!("{n} hidden replies in {days} days. Latest: {}", reply["text"].as_str().unwrap_or(""));
                self.notify(&format!("Block @{username}?"), &message, &format!("https://www.threads.com/@{username}"));
            }
            Ok(_) => {}
            Err(e) => log(json!({"level": "warning", "msg": "strike count failed", "username": username, "error": e})),
        }
    }

    /// A reply the app hid and you unhid: count it (false positives, for tuning
    /// the thresholds) and stop hiding copies of its text.
    fn note_unhidden(&self, reply: &Value) -> Result<(), String> {
        let Some(id) = reply["id"].as_str() else { return Ok(()) };
        let Some((category, bad_p)) = self.store.unhide(id)? else { return Ok(()) };
        let text = reply["text"].as_str().unwrap_or("");
        if let Some(k) = copy_key(text) {
            self.store.put(&k, "");
        }
        *metrics().unhidden.entry(category.clone()).or_default() += 1;
        log(json!({
            "level": "info", "msg": "unhidden", "reply_id": id, "username": reply["username"], "text": text,
            "category": category, "bad_probability": (bad_p * 10000.0).round() / 10000.0,
            "permalink": reply["permalink"].as_str().unwrap_or(""),
        }));
        Ok(())
    }

    /// Retry hides that failed earlier. A rate limit stops here; a 4xx (the reply
    /// is gone, for example) gives up as hide_failed; anything else stays pending.
    fn retry_pending_hides(&self) -> Result<(), String> {
        if self.dry_run {
            return Ok(());
        }
        for p in self.store.pending_hides()? {
            let action = match self.threads().hide(&p.id) {
                Ok(()) => "hidden",
                Err(e) if is_rate_limited(&e) => return Err(e),
                Err(e) if is_client_error(&e) => {
                    log(json!({"level": "error", "msg": "hide failed for good", "reply_id": p.id, "error": e}));
                    "hide_failed"
                }
                Err(e) => {
                    log(json!({"level": "warning", "msg": "hide failed, will retry", "reply_id": p.id, "error": e}));
                    continue;
                }
            };
            self.store.set_action(&p.id, action)?;
            *metrics().replies.entry((p.category.clone(), action.into(), p.stage.clone())).or_default() += 1;
            log(json!({
                "level": "info", "msg": "decision", "dry_run": false, "retried": true,
                "reply_id": p.id, "post_id": p.post_id, "username": p.username, "text": p.text,
                "category": p.category, "bad_probability": (p.bad_p * 10000.0).round() / 10000.0,
                "action": action, "stage": p.stage, "reason": p.reason, "permalink": "",
            }));
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<(), String> {
        self.retry_pending_hides()?;
        let posts = self.threads().posts(now().saturating_sub(self.lookback))?;
        let ids: HashSet<&str> = posts.iter().filter_map(|p| p["id"].as_str()).collect();
        self.next_check.retain(|id, _| ids.contains(id.as_str()));
        self.last_activity.retain(|id, _| ids.contains(id.as_str()));
        // Reposts show up in your own feed, but their replies belong to the
        // original author's thread and cannot be hidden by you.
        let own: Vec<&Value> = posts
            .iter()
            .filter(|p| p["media_type"].as_str() != Some("REPOST_FACADE"))
            .filter(|p| matches_filter(p, &self.topics, &self.hashtags))
            .collect();
        let (due, not_due, over_budget) = select_due(own, &self.next_check, now(), self.max_posts_per_poll);
        *metrics().post_checks.entry("skipped").or_default() += not_due as u64;
        *metrics().post_checks.entry("over_budget").or_default() += over_budget as u64;
        for post in due {
            let Some(post_id) = post["id"].as_str() else { continue };
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
                if reply["hide_status"].as_str() == Some("UNHUSHED") {
                    self.note_unhidden(reply)?;
                }
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
                // A rate limit stops the whole poll.
                if let Err(e) = self.handle(post, reply) {
                    if is_rate_limited(&e) {
                        return Err(e);
                    }
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
            if now() < self.paused_until {
                thread::sleep(Duration::from_secs(self.interval.min(self.paused_until.saturating_sub(now())).max(1)));
                continue;
            }
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
                Ok(()) => {
                    metrics().last_poll = now();
                    self.backoff = 0;
                }
                Err(e) if is_rate_limited(&e) => {
                    self.backoff = next_backoff(self.backoff, self.min_backoff, self.max_backoff);
                    self.paused_until = now() + self.backoff;
                    metrics().paused_until = self.paused_until;
                    log(json!({"level": "warning", "msg": "rate limited, pausing", "seconds": self.backoff, "until": self.paused_until, "error": e}));
                }
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

/// Posts whose check is due, most overdue first (never checked = first), capped
/// at `budget` (0: no cap). Also returns how many were not due and how many
/// were due but over the budget; those stay due for the next poll.
fn select_due<'a>(posts: Vec<&'a Value>, next_check: &HashMap<String, u64>, now: u64, budget: usize) -> (Vec<&'a Value>, usize, usize) {
    let due_at = |p: &Value| p["id"].as_str().and_then(|id| next_check.get(id)).copied().unwrap_or(0);
    let total = posts.len();
    let mut due: Vec<&Value> = posts.into_iter().filter(|p| due_at(p) <= now).collect();
    let not_due = total - due.len();
    due.sort_by_key(|p| due_at(p));
    let over = if budget > 0 && due.len() > budget { due.len() - budget } else { 0 };
    due.truncate(due.len() - over);
    (due, not_due, over)
}

/// HIDE_PATTERN as a case-insensitive regex; None when empty.
fn hide_pattern(spec: &str) -> Option<Regex> {
    (!spec.is_empty()).then(|| Regex::new(&format!("(?i){spec}")).unwrap_or_else(|e| fatal(&format!("invalid HIDE_PATTERN: {e}"))))
}

/// "harassment=Please keep it civil.|sexual=..." -> category -> answer.
/// None for an unknown category or an empty answer.
fn parse_auto_reply(spec: &str) -> Option<HashMap<String, String>> {
    spec.split('|')
        .filter(|e| !e.trim().is_empty())
        .map(|e| {
            let (c, t) = e.split_once('=')?;
            let (c, t) = (c.trim().to_lowercase(), t.trim());
            (classifier::CATEGORIES.iter().any(|(k, _)| *k == c) && !t.is_empty()).then(|| (c, t.to_string()))
        })
        .collect()
}

/// kv key for a reply's text without case, spaces, punctuation or emoji, so copies
/// of a hidden reply match. None for short texts, which repeat innocently ("mantap kak").
// ponytail: exact match after normalizing; a wave that varies one word gets past, fuzzy hashing if that happens.
fn copy_key(text: &str) -> Option<String> {
    let norm: String = text.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect();
    (norm.chars().count() >= 20).then(|| format!("copy:{norm}"))
}

/// The next pause after a rate limit: the minimum first, then doubling, capped.
fn next_backoff(current: u64, min: u64, max: u64) -> u64 {
    (current * 2).clamp(min, max)
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

    #[test]
    fn due_posts_and_backoff() {
        let posts = [json!({"id": "a"}), json!({"id": "b"}), json!({"id": "c"}), json!({"id": "d"})];
        let next: HashMap<String, u64> = [("a", 50), ("b", 10), ("c", 500)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let ids = |v: Vec<&Value>| v.iter().map(|p| p["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        // c is not due; d was never checked so it goes first, then the most overdue.
        let (due, not_due, over) = select_due(posts.iter().collect(), &next, 100, 0);
        assert_eq!((ids(due), not_due, over), (vec!["d".to_string(), "b".into(), "a".into()], 1, 0));
        let (due, _, over) = select_due(posts.iter().collect(), &next, 100, 2);
        assert_eq!((ids(due), over), (vec!["d".to_string(), "b".into()], 1));
        assert_eq!(next_backoff(0, 900, 21_600), 900);
        assert_eq!(next_backoff(900, 900, 21_600), 1800);
        assert_eq!(next_backoff(14_400, 900, 21_600), 21_600);
    }

    // Runs every handle() case in one test: they share process env vars.
    #[test]
    fn handle_actions() {
        let setup = |dry_run: bool, probs: Value, verdict: Option<&str>, hide_err: Option<&'static str>| -> (Monitor, Rc<RefCell<Vec<&'static str>>>) {
            let calls = Rc::new(RefCell::new(Vec::new()));
            let (c, verdict_text) = (calls.clone(), verdict.map(String::from));
            // The first hide call fails with `hide_err`, later ones succeed.
            let failed_once = Rc::new(RefCell::new(false));
            let http: Http = Box::new(move |endpoint, _, _, _, _| {
                c.borrow_mut().push(endpoint);
                let already_failed = *failed_once.borrow();
                if let (Some(e), "hide", false) = (hide_err, endpoint, already_failed) {
                    *failed_once.borrow_mut() = true;
                    return Err(e.into());
                }
                Ok(match endpoint {
                    "classifier" => json!({"result": {"answers": {"category": {"probabilities": probs}}}}),
                    "reviewer" => json!({"choices": [{"message": {"content": verdict_text.clone().unwrap_or_default()}}]}),
                    "reply" => json!({"id": "container"}),
                    "drafter" => json!({"choices": [{"message": {"content": "{\"reply\": \"Santai aja kak\"}"}}]}),
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
            (Monitor::from_env(http), calls)
        };
        let post = json!({"id": "p1", "text": "post"});
        let reply = json!({"id": "r1", "text": "slot gacor", "username": "x"});
        let run = |dry_run: bool, probs: Value, verdict: Option<&str>| -> (String, Vec<&'static str>) {
            let (m, calls) = setup(dry_run, probs, verdict, None);
            m.handle(&post, &reply).unwrap();
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
        assert_eq!(run(false, gray.clone(), Some("no idea")), ("review".into(), vec!["classifier", "reviewer"]));

        // Rate-limited hide: saved as hide_pending, the poll is stopped, and the
        // retry hides it without classifying again.
        let (m, calls) = setup(false, json!({"spam": 0.99}), None, Some("hide: rate limited (HTTP 429): "));
        assert!(is_rate_limited(&m.handle(&post, &reply).unwrap_err()));
        assert_eq!(m.store.action("r1").as_deref(), Some("hide_pending"));
        m.retry_pending_hides().unwrap();
        assert_eq!(m.store.action("r1").as_deref(), Some("hidden"));
        assert_eq!(*calls.borrow(), vec!["classifier", "hide", "hide"]);

        // The reply is gone (4xx): the retry gives up as hide_failed.
        let (m, _) = setup(false, json!({"spam": 0.99}), None, Some("hide: HTTP 500: busy"));
        m.handle(&post, &reply).unwrap(); // not a rate limit: the poll goes on
        assert_eq!(m.store.action("r1").as_deref(), Some("hide_pending"));
        m.store.set_action("r1", "hide_pending").unwrap();
        let gone: Http = Box::new(|_, _, _, _, _| Err("hide: HTTP 400: {\"error\":{\"code\":100}}".into()));
        let m = Monitor { http: gone, ..m };
        m.retry_pending_hides().unwrap();
        assert_eq!(m.store.action("r1").as_deref(), Some("hide_failed"));

        // Copypasta: a copy of a hidden reply is hidden without the classifier.
        let (m, calls) = setup(false, json!({"spam": 0.99}), None, None);
        let spam = |id: &str, text: &str| json!({"id": id, "text": text, "username": "x", "permalink": "p"});
        m.handle(&post, &spam("c1", "Slot gacor hari ini, daftar di sini!")).unwrap();
        m.handle(&post, &spam("c2", "slot GACOR hari ini 🔥 daftar di sini")).unwrap();
        assert_eq!(m.store.action("c2").as_deref(), Some("hidden"));
        assert_eq!(*calls.borrow(), vec!["classifier", "hide", "hide"]);
        // Third strike for x within the window: block_suggested.
        let before = metrics().block_suggested;
        m.handle(&post, &spam("c3", "another long spam reply with a link")).unwrap();
        assert_eq!(metrics().block_suggested, before + 1);
        // You unhide c1: counted once, and its copies go to the classifier again.
        m.note_unhidden(&json!({"id": "c1", "text": "Slot gacor hari ini, daftar di sini!", "hide_status": "UNHUSHED"})).unwrap();
        assert_eq!(m.store.action("c1").as_deref(), Some("unhidden"));
        assert!(m.store.unhide("c1").unwrap().is_none());
        calls.borrow_mut().clear();
        m.handle(&post, &spam("c4", "slot gacor hari ini daftar di sini")).unwrap();
        assert_eq!(*calls.borrow(), vec!["classifier", "hide"]);

        // HIDE_PATTERN hides without the classifier; a review is notified.
        env::set_var("HIDE_PATTERN", r"gacor|wa\.me/\d+");
        env::set_var("NOTIFY_URL", "https://ntfy.sh/btw");
        let (m, calls) = setup(false, gray.clone(), None, None);
        m.handle(&post, &spam("h1", "Slot GACOR")).unwrap();
        assert_eq!(m.store.action("h1").as_deref(), Some("hidden"));
        m.handle(&post, &spam("h2", "is this ok?")).unwrap();
        assert_eq!(m.store.action("h2").as_deref(), Some("review"));
        assert_eq!(*calls.borrow(), vec!["hide", "classifier", "notify"]);
        env::remove_var("HIDE_PATTERN");
        env::remove_var("NOTIFY_URL");

        // AUTO_REPLY answers a visible reply once per author; a hidden one gets no answer.
        env::set_var("AUTO_REPLY", "harassment=Please keep it civil.");
        env::set_var("AUTO_REPLY_MODE", "post");
        let rude = json!({"harassment": 0.6, "neutral": 0.4});
        let (m, calls) = setup(false, rude.clone(), None, None);
        m.handle(&post, &spam("a1", "you are an idiot")).unwrap();
        m.handle(&post, &spam("a2", "still an idiot")).unwrap();
        assert_eq!(m.store.action("a2").as_deref(), Some("review"));
        assert_eq!(*calls.borrow(), vec!["classifier", "reply", "reply", "classifier"]);
        let (m, calls) = setup(false, json!({"harassment": 0.99}), None, None);
        m.handle(&post, &spam("a3", "you are an idiot")).unwrap();
        assert_eq!(*calls.borrow(), vec!["classifier", "hide"]);
        let (m, calls) = setup(true, rude, None, None);
        m.handle(&post, &spam("a4", "you are an idiot")).unwrap();
        assert_eq!(*calls.borrow(), vec!["classifier"]);
        // With a reviewer, the model drafts the answer, and the classifier checks
        // the draft: here it finds it as bad as the reply, so the base answer is posted.
        let (m, calls) = setup(false, json!({"harassment": 0.6, "neutral": 0.4}), Some("{\"action\": \"keep\"}"), None);
        m.handle(&post, &spam("a5", "you are an idiot")).unwrap();
        assert_eq!(*calls.borrow(), vec!["classifier", "reviewer", "drafter", "classifier", "reply", "reply"]);
        let input = Input { post: "post", reply: "you are an idiot", author: "x" };
        assert!(m.check_draft(&input, "Santai aja kak").is_err());
        let (m, _) = setup(false, json!({"neutral": 0.9, "spam": 0.1}), None, None);
        assert!(m.check_draft(&input, "Santai aja kak").is_ok());
        let (m, _) = setup(false, json!({"spam": 0.4, "neutral": 0.3, "positive": 0.3}), None, None);
        assert!(m.check_draft(&input, "Santai aja kak").is_err());
        env::set_var("HIDE_PATTERN", r"wa\.me");
        let (m, calls) = setup(false, json!({"neutral": 0.9}), None, None);
        assert!(m.check_draft(&input, "chat me at WA.me/62812").is_err());
        assert!(calls.borrow().is_empty());
        env::remove_var("HIDE_PATTERN");
        // Draft mode: the answer is pushed to you, not posted, also in a dry run.
        env::set_var("AUTO_REPLY_MODE", "draft");
        env::set_var("NOTIFY_URL", "https://ntfy.sh/btw");
        let (m, calls) = setup(true, json!({"harassment": 0.6, "neutral": 0.4}), None, None);
        m.handle(&post, &spam("a6", "you are an idiot")).unwrap();
        assert_eq!(*calls.borrow(), vec!["classifier", "notify", "notify"]);
        env::remove_var("NOTIFY_URL");
        env::remove_var("AUTO_REPLY_MODE");
        env::remove_var("AUTO_REPLY");
    }

    #[test]
    fn auto_reply_spec() {
        let m = parse_auto_reply(" harassment = Please keep it civil. | Sexual=Not here.|").unwrap();
        assert_eq!(m["harassment"], "Please keep it civil.");
        assert_eq!(m["sexual"], "Not here.");
        assert!(parse_auto_reply("").unwrap().is_empty());
        assert!(parse_auto_reply("review=hi").is_none());
        assert!(parse_auto_reply("spam=").is_none());
        assert!(parse_auto_reply("no equals sign").is_none());
    }

    #[test]
    fn copy_keys() {
        assert_eq!(copy_key("Slot GACOR hari ini!! 🔥 daftar"), copy_key("slot gacor hari ini daftar"));
        assert_eq!(copy_key("mantap kak 🔥"), None);
    }
}
