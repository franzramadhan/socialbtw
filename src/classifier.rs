//! Stage 1: classify a reply into categories with probabilities.
//!
//! Any model that can do this plugs in behind `Classifier`:
//! - `systemone`: decision models in the System One format (state + typed
//!   questions), such as Cloudflare Clef or TypeSafe Jev, directly or through
//!   LiteLLM's /v1/systemone
//! - `chat`: any OpenAI-compatible chat model (Claude or GPT through LiteLLM,
//!   Ollama, ...) asked for JSON probabilities

use crate::http::Http;
use crate::llm::Chat;
use crate::util::{extract_json, fatal, var};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub type Probabilities = BTreeMap<String, f64>;

/// Categories that get a reply hidden.
pub const BAD: [&str; 4] = ["hateful", "harassment", "spam", "sexual"];

/// Every category with the definition the models see.
pub const CATEGORIES: [(&str, &str); 6] = [
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

const INSTRUCTIONS: &str = "Classify the reply in the context of the post. \
                            The reply may be in Indonesian, English, or mixed, and may use slang.";

/// The categories as a bullet list, for chat prompts.
pub fn categories_text() -> String {
    CATEGORIES.iter().map(|(c, d)| format!("- {c}: {d}")).collect::<Vec<_>>().join("\n")
}

/// What a classifier looks at.
pub struct Input<'a> {
    pub post: &'a str,
    pub reply: &'a str,
    pub author: &'a str,
}

pub trait Classifier {
    /// Probability per category in CATEGORIES.
    fn classify(&self, http: &Http, input: &Input) -> Result<Probabilities, String>;
}

/// CLASSIFIER selects the backend; CLASSIFIER_URL, CLASSIFIER_TOKEN and
/// CLASSIFIER_MODEL configure it.
pub fn from_env(extra_rules: &str) -> Box<dyn Classifier> {
    let url = var("CLASSIFIER_URL", None);
    let token = var("CLASSIFIER_TOKEN", Some(""));
    match var("CLASSIFIER", Some("systemone")).as_str() {
        "systemone" => Box::new(SystemOne { url, token, model: var("CLASSIFIER_MODEL", Some("clef")) }),
        "chat" => Box::new(ChatClassifier { chat: Chat::new(&url, &token, &var("CLASSIFIER_MODEL", None)), extra_rules: extra_rules.into() }),
        other => fatal(&format!("unknown CLASSIFIER {other:?}: use systemone or chat")),
    }
}

/// System One decision models: one `choice` question over CATEGORIES.
pub struct SystemOne {
    /// Full endpoint, e.g. https://api.cloudflare.com/client/v4/accounts/<id>/ai/run/@cf/cloudflare/clef
    /// or http://litellm:4000/v1/systemone
    pub url: String,
    pub token: String,
    pub model: String,
}

impl Classifier for SystemOne {
    fn classify(&self, http: &Http, input: &Input) -> Result<Probabilities, String> {
        let criteria: Map<String, Value> = CATEGORIES.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let body = json!({
            "model": self.model,
            "state": {"post": input.post, "reply": input.reply, "reply_author": input.author},
            "questions": {"category": {"type": "choice", "instructions": INSTRUCTIONS, "criteria": criteria}},
        });
        let bearer = (!self.token.is_empty()).then_some(self.token.as_str());
        let resp = http("classifier", "POST", &self.url, bearer, Some(&body))?;
        // Cloudflare wraps results in {"result": ...}; LiteLLM and Jev do not.
        let result = resp.get("result").unwrap_or(&resp);
        result["answers"]["category"]["probabilities"]
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_f64().unwrap_or(0.0))).collect())
            .ok_or_else(|| format!("classifier: unexpected response: {}", result.to_string().chars().take(300).collect::<String>()))
    }
}

/// Chat models: asked for a JSON object of probabilities, normalized to sum to 1.
pub struct ChatClassifier {
    pub chat: Chat,
    pub extra_rules: String,
}

impl Classifier for ChatClassifier {
    fn classify(&self, http: &Http, input: &Input) -> Result<Probabilities, String> {
        let system = format!(
            "You classify replies on the author's own Threads posts. {INSTRUCTIONS}\n\
             Categories:\n{}\n{}\n\
             Answer with JSON only: an object with every category name as a key and its \
             probability from 0 to 1 as the value, summing to 1.",
            categories_text(),
            self.extra_rules
        );
        let user = json!({"post": input.post, "reply": input.reply, "reply_author": input.author});
        parse_probabilities(&self.chat.ask(http, "classifier", &system, &user)?)
    }
}

fn parse_probabilities(content: &str) -> Result<Probabilities, String> {
    let v = extract_json(content).ok_or_else(|| format!("classifier: no JSON in answer: {}", content.chars().take(200).collect::<String>()))?;
    // Accept {"spam": 0.9, ...} or {"probabilities": {"spam": 0.9, ...}}.
    let v = if v["probabilities"].is_object() { &v["probabilities"] } else { &v };
    let mut p: Probabilities = CATEGORIES.iter().map(|(c, _)| (c.to_string(), v[c].as_f64().unwrap_or(0.0).clamp(0.0, 1.0))).collect();
    let sum: f64 = p.values().sum();
    if sum <= 0.0 {
        return Err(format!("classifier: no category probabilities in answer: {}", content.chars().take(200).collect::<String>()));
    }
    p.values_mut().for_each(|x| *x /= sum);
    Ok(p)
}

/// Map probabilities to (category, bad probability, action): "hide", "escalate"
/// (ask the reviewer) or "keep".
///
/// Bad probability is summed across bad categories, so a reply split between
/// "hateful" and "harassment" still counts as clearly bad.
pub fn decide(p: &Probabilities, hide: f64, review: f64) -> (String, f64, &'static str) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn probs(pairs: &[(&str, f64)]) -> Probabilities {
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
    fn chat_answers() {
        let p = parse_probabilities("```json\n{\"spam\": 0.9, \"neutral\": 0.3}\n```").unwrap();
        assert!((p["spam"] - 0.75).abs() < 1e-9); // normalized to sum to 1
        assert_eq!(p["hateful"], 0.0);
        assert_eq!(parse_probabilities("{\"spam\": 3}").unwrap()["spam"], 1.0); // clamped
        let p = parse_probabilities("{\"probabilities\": {\"positive\": 0.8, \"neutral\": 0.2}}").unwrap();
        assert!((p["positive"] - 0.8).abs() < 1e-9);
        assert!(parse_probabilities("{\"verdict\": \"spam\"}").is_err());
        assert!(parse_probabilities("no idea").is_err());
    }

    // Needs a chat model: TEST_CHAT_URL (base URL), TEST_CHAT_MODEL, TEST_CHAT_KEY.
    // Skipped otherwise.
    #[test]
    fn live_chat_classifier() {
        let Ok(url) = std::env::var("TEST_CHAT_URL") else { return };
        let c = ChatClassifier {
            chat: Chat::new(&url, &std::env::var("TEST_CHAT_KEY").unwrap_or_default(), &std::env::var("TEST_CHAT_MODEL").unwrap()),
            extra_rules: String::new(),
        };
        let http = crate::http::real_http();
        let post = "Domain .id makin mahal ya";
        let ad = c.classify(&http, &Input { post, reply: "pakai .ai.id di anekahost.com cuma 80.000 pertahun kak", author: "anekahost" }).unwrap();
        let banter = c.classify(&http, &Input { post: "Abis lari pagi 10k", reply: "Spil treadmillnya bang", author: "x" }).unwrap();
        eprintln!("ad={ad:?}\nbanter={banter:?}");
        assert_eq!(decide(&ad, 0.85, 0.5).0, "spam");
        assert!(decide(&banter, 0.85, 0.5).1 < 0.5);
    }

    #[test]
    fn backends_parse_their_answers() {
        let input = Input { post: "p", reply: "r", author: "a" };
        let cf: Http = Box::new(|_, _, _, _, _| Ok(json!({"result": {"answers": {"category": {"probabilities": {"spam": 0.9}}}}})));
        let s1 = SystemOne { url: "http://x".into(), token: String::new(), model: "clef".into() };
        assert_eq!(s1.classify(&cf, &input).unwrap()["spam"], 0.9);
        let chat: Http = Box::new(|_, _, _, _, _| Ok(json!({"choices": [{"message": {"content": "{\"hateful\": 1}"}}]})));
        let c = ChatClassifier { chat: Chat::new("http://x/v1/", "", "m"), extra_rules: String::new() };
        assert_eq!(c.classify(&chat, &input).unwrap()["hateful"], 1.0);
    }
}
