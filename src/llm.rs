//! OpenAI-compatible chat completions (LiteLLM, OpenAI, Ollama, vLLM, ...),
//! shared by the chat classifier and the reviewer.

use crate::http::Http;
use serde_json::{json, Value};

pub struct Chat {
    /// Base URL, e.g. http://litellm:4000/v1 (".../chat/completions" is appended).
    pub url: String,
    pub key: String,
    pub model: String,
}

impl Chat {
    pub fn new(url: &str, key: &str, model: &str) -> Self {
        Chat { url: url.trim_end_matches('/').into(), key: key.into(), model: model.into() }
    }

    /// One system + user turn at temperature 0; returns the answer text.
    pub fn ask(&self, http: &Http, endpoint: &'static str, system: &str, user: &Value) -> Result<String, String> {
        let body = json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user.to_string()},
            ],
        });
        let bearer = (!self.key.is_empty()).then_some(self.key.as_str());
        let resp = http(endpoint, "POST", &format!("{}/chat/completions", self.url), bearer, Some(&body))?;
        Ok(resp["choices"][0]["message"]["content"].as_str().unwrap_or("").into())
    }
}
