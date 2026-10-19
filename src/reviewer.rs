//! Stage 2 (optional): a chat model gives a second opinion on replies the
//! classifier was unsure about, and answers hide, keep or review.

use crate::classifier::{categories_text, Input, Probabilities};
use crate::http::Http;
use crate::llm::Chat;
use crate::util::{extract_json, var};
use serde_json::json;

pub struct Reviewer {
    pub chat: Chat,
    pub extra_rules: String,
}

impl Reviewer {
    /// LLM_BASE_URL and LLM_MODEL set: enabled (LLM_API_KEY optional).
    pub fn from_env(extra_rules: &str) -> Option<Self> {
        let (url, model) = (var("LLM_BASE_URL", Some("")), var("LLM_MODEL", Some("")));
        (!url.is_empty() && !model.is_empty())
            .then(|| Reviewer { chat: Chat::new(&url, &var("LLM_API_KEY", Some("")), &model), extra_rules: extra_rules.into() })
    }

    /// (action, reason), action one of hide, keep, review.
    pub fn review(&self, http: &Http, input: &Input, p: &Probabilities) -> Result<(String, String), String> {
        let system = format!(
            "You moderate replies on the author's own Threads posts. Replies may be in \
             Indonesian, English, or a mix, and may use slang.\nCategories:\n{}\n\
             Hide replies that are hateful, harassment, spam or sexual. Keep criticism, \
             disagreement, and friendly jokes. {}\n\
             Answer with JSON only: {{\"action\": \"hide\" | \"keep\" | \"review\", \"reason\": \"<one short sentence>\"}}.\n\
             Use \"review\" only when you truly cannot decide.",
            categories_text(),
            self.extra_rules
        );
        let user = json!({"post": input.post, "reply": input.reply, "reply_author": input.author, "classifier_probabilities": p});
        Ok(parse_verdict(&self.chat.ask(http, "reviewer", &system, &user)?))
    }
}

fn parse_verdict(content: &str) -> (String, String) {
    match extract_json(content) {
        Some(v) => {
            let action = match v["action"].as_str() {
                Some(a @ ("hide" | "keep" | "review")) => a,
                _ => "review",
            };
            (action.into(), v["reason"].as_str().unwrap_or("").into())
        }
        None => ("review".into(), format!("unparseable reviewer answer: {}", content.chars().take(200).collect::<String>())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts() {
        assert_eq!(parse_verdict("```json\n{\"action\": \"hide\", \"reason\": \"promo\"}\n```"), ("hide".into(), "promo".into()));
        assert_eq!(parse_verdict("{\"action\": \"delete\"}").0, "review");
        assert_eq!(parse_verdict("no idea").0, "review");
    }
}
