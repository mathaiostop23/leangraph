//! Which API the model calls speak.
//!
//! The agent asks the same four things of every provider — classify this issue,
//! explain this bug, propose this patch, and how much did that cost — and the
//! providers disagree about all of the spelling and some of the substance. This
//! module holds the disagreement so the prompts do not have to.
//!
//! What is *not* abstracted, deliberately:
//!
//! * **Prompt caching.** Anthropic takes an explicit breakpoint and reports what
//!   was read and written; OpenAI caches prefixes on its own terms and reports
//!   only a count. A common interface over those two would have to either
//!   discard the breakpoint, which is the measured feature here, or invent one
//!   for OpenAI that does nothing. So `caching` is a capability a provider
//!   either has or does not, and the ledger reads zero where it does not.
//! * **The Batch API.** Both offer one at half price and they are not the same
//!   shape. Backfill stays Anthropic-only until the second is built and
//!   measured, rather than shipping a path nobody has run.
//!
//! Saying which of the published numbers depend on those: the cache-hit figures
//! and the half-price backfill in BENCH.md are Anthropic measurements and remain
//! so.

use serde_json::{json, Value};

/// What a call is *for*. The provider picks the model, because "the cheap one"
/// and "the one that reasons" are the stable ideas and the ids are not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Extraction, not reasoning: classify and pull out symbol names.
    Triage,
    /// The explanation an issue gets answered with.
    Analyse,
    /// A patch.
    Fix,
    /// Where low self-reported confidence is sent.
    Escalate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

/// One block of system text. `cached` marks the end of the stable prefix — the
/// last point a provider that takes a breakpoint should be told about.
pub struct Block {
    pub text: String,
    pub cached: bool,
}

/// A request, before anyone has decided what JSON it becomes.
pub struct Ask {
    pub max_tokens: u32,
    pub system: Vec<Block>,
    pub user: String,
    /// Structured output, where the answer has to be machine-read.
    pub schema: Option<Value>,
}

impl Provider {
    /// From configuration, or from whichever key exists.
    ///
    /// Inferring from the key is what makes a single-key install work with no
    /// configuration at all, which is the common case; naming it explicitly is
    /// what makes a two-key install deterministic.
    pub fn choose(named: Option<&str>, anthropic_key: bool, openai_key: bool) -> Option<Provider> {
        match named.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("anthropic") => return Some(Provider::Anthropic),
            Some("openai") => return Some(Provider::OpenAi),
            Some(other) if !other.is_empty() => return None,
            _ => {}
        }
        match (anthropic_key, openai_key) {
            (true, _) => Some(Provider::Anthropic),
            (false, true) => Some(Provider::OpenAi),
            (false, false) => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
        }
    }

    pub fn default_base(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
            Provider::OpenAi => "https://api.openai.com/v1/chat/completions",
        }
    }

    /// An explicit breakpoint the caller can place, and a ledger that can report
    /// what it bought. OpenAI caches without being asked and reports only reads,
    /// which is not the same feature and is not pretended to be.
    pub fn caching(self) -> bool {
        matches!(self, Provider::Anthropic)
    }

    /// Backfill through a half-price batch endpoint.
    pub fn batch(self) -> bool {
        matches!(self, Provider::Anthropic)
    }

    pub fn model(self, role: Role) -> &'static str {
        match (self, role) {
            (Provider::Anthropic, Role::Triage) => "claude-haiku-4-5",
            (Provider::Anthropic, Role::Analyse) => "claude-sonnet-5",
            (Provider::Anthropic, Role::Fix) => "claude-sonnet-5",
            (Provider::Anthropic, Role::Escalate) => "claude-opus-5",
            (Provider::OpenAi, Role::Triage) => "gpt-4.1-mini",
            (Provider::OpenAi, Role::Analyse) => "gpt-4.1",
            (Provider::OpenAi, Role::Fix) => "gpt-4.1",
            (Provider::OpenAi, Role::Escalate) => "o3",
        }
    }

    pub fn auth_header(self) -> &'static str {
        match self {
            Provider::Anthropic => "x-api-key",
            Provider::OpenAi => "authorization",
        }
    }

    pub fn auth_value(self, key: &str) -> String {
        match self {
            Provider::Anthropic => key.to_string(),
            Provider::OpenAi => format!("Bearer {key}"),
        }
    }

    /// The request body this provider expects.
    pub fn render(self, ask: &Ask, model: &str) -> Value {
        match self {
            Provider::Anthropic => {
                let system: Vec<Value> = ask
                    .system
                    .iter()
                    .map(|b| {
                        if b.cached {
                            json!({ "type": "text", "text": b.text,
                                    "cache_control": { "type": "ephemeral", "ttl": "1h" } })
                        } else {
                            json!({ "type": "text", "text": b.text })
                        }
                    })
                    .collect();
                let mut v = json!({
                    "model": model,
                    "max_tokens": ask.max_tokens,
                    "system": system,
                    "messages": [{ "role": "user", "content": ask.user }],
                });
                if let Some(s) = &ask.schema {
                    v["output_config"] =
                        json!({ "format": { "type": "json_schema", "schema": s } });
                }
                v
            }
            Provider::OpenAi => {
                // One system message rather than blocks: the wire format has no
                // notion of them, and joining preserves the order the prompt was
                // written in, which is what the order meant.
                let system = ask
                    .system
                    .iter()
                    .map(|b| b.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let mut v = json!({
                    "model": model,
                    // Not `max_tokens`: that name is deprecated on the models
                    // this would actually be pointed at, and is rejected by some.
                    "max_completion_tokens": ask.max_tokens,
                    "messages": [
                        { "role": "system", "content": system },
                        { "role": "user", "content": ask.user },
                    ],
                });
                if let Some(s) = &ask.schema {
                    v["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": { "name": "answer", "strict": true, "schema": s }
                    });
                }
                v
            }
        }
    }

    /// Did the model decline, as opposed to fail?
    pub fn refused(self, v: &Value) -> bool {
        match self {
            Provider::Anthropic => v.get("stop_reason").and_then(Value::as_str) == Some("refusal"),
            Provider::OpenAi => v
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("refusal"))
                .is_some_and(|r| !r.is_null()),
        }
    }

    /// The text of the answer, and what it cost.
    pub fn parse(self, v: &Value) -> (String, i64, i64, i64, i64, String) {
        let model = v
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match self {
            Provider::Anthropic => {
                let text = v
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("")
                    })
                    .unwrap_or_default();
                let u = v.get("usage").cloned().unwrap_or_else(|| json!({}));
                let g = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
                (
                    text,
                    g("input_tokens"),
                    g("output_tokens"),
                    g("cache_read_input_tokens"),
                    g("cache_creation_input_tokens"),
                    model,
                )
            }
            Provider::OpenAi => {
                let text = v
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let u = v.get("usage").cloned().unwrap_or_else(|| json!({}));
                let g = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
                let cached = u
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                // `prompt_tokens` counts the cached part too, so subtracting it
                // makes the two providers' "input" mean the same thing: what was
                // paid for at the full rate.
                let input = (g("prompt_tokens") - cached).max(0);
                // No write figure exists: nothing was asked to be written.
                (text, input, g("completion_tokens"), cached, 0, model)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask() -> Ask {
        Ask {
            max_tokens: 512,
            system: vec![
                Block {
                    text: "rules".into(),
                    cached: false,
                },
                Block {
                    text: "repo preamble".into(),
                    cached: true,
                },
            ],
            user: "<issue>\ntitle: t\n\nb\n</issue>".into(),
            schema: None,
        }
    }

    #[test]
    fn a_named_provider_wins_over_whichever_key_exists() {
        assert_eq!(
            Provider::choose(Some("openai"), true, false),
            Some(Provider::OpenAi)
        );
        assert_eq!(
            Provider::choose(Some("anthropic"), false, true),
            Some(Provider::Anthropic)
        );
        // A name nobody recognises is a configuration error, not a default.
        assert_eq!(Provider::choose(Some("gemini"), true, true), None);
    }

    #[test]
    fn with_no_name_the_key_decides_and_no_key_means_no_provider() {
        assert_eq!(Provider::choose(None, false, true), Some(Provider::OpenAi));
        assert_eq!(
            Provider::choose(None, true, false),
            Some(Provider::Anthropic)
        );
        assert_eq!(Provider::choose(None, false, false), None);
    }

    #[test]
    fn the_cache_breakpoint_is_placed_only_where_it_is_read_back() {
        let a = Provider::Anthropic.render(&ask(), "m");
        let blocks = a["system"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[1].get("cache_control").is_some(), "{blocks:?}");

        // OpenAI has no breakpoint to place, so the blocks become one string
        // and the order is what carries the meaning.
        let o = Provider::OpenAi.render(&ask(), "m");
        let sys = o["messages"][0]["content"].as_str().unwrap();
        assert!(sys.starts_with("rules"), "{sys}");
        assert!(sys.contains("repo preamble"), "{sys}");
        assert!(!o.to_string().contains("cache_control"));
    }

    #[test]
    fn the_output_cap_uses_each_wire_formats_own_name() {
        assert!(Provider::Anthropic
            .render(&ask(), "m")
            .get("max_tokens")
            .is_some());
        let o = Provider::OpenAi.render(&ask(), "m");
        assert!(o.get("max_completion_tokens").is_some());
        assert!(
            o.get("max_tokens").is_none(),
            "the deprecated name must not be sent"
        );
    }

    #[test]
    fn input_tokens_mean_the_same_thing_on_both() {
        // Anthropic reports the uncached input and the cache read separately.
        let (_, input, out, read, write, _) = Provider::Anthropic.parse(&json!({
            "content": [{"type":"text","text":"hi"}],
            "usage": {"input_tokens": 100, "output_tokens": 7,
                      "cache_read_input_tokens": 900, "cache_creation_input_tokens": 0}
        }));
        assert_eq!((input, out, read, write), (100, 7, 900, 0));

        // OpenAI folds the cache read into `prompt_tokens`, so it is subtracted
        // out — otherwise the same conversation would look ten times dearer on
        // one provider than the other.
        let (text, input, out, read, write, _) = Provider::OpenAi.parse(&json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 900}}
        }));
        assert_eq!(text, "hi");
        assert_eq!((input, out, read, write), (100, 7, 900, 0));
    }

    #[test]
    fn a_refusal_is_recognised_on_both_and_a_normal_answer_is_not() {
        assert!(Provider::Anthropic.refused(&json!({"stop_reason": "refusal"})));
        assert!(!Provider::Anthropic.refused(&json!({"stop_reason": "end_turn"})));
        assert!(Provider::OpenAi.refused(&json!({"choices":[{"message":{"refusal":"no"}}]})));
        assert!(!Provider::OpenAi
            .refused(&json!({"choices":[{"message":{"content":"yes","refusal":null}}]})));
    }

    #[test]
    fn only_the_provider_that_has_them_claims_caching_and_batch() {
        assert!(Provider::Anthropic.caching() && Provider::Anthropic.batch());
        assert!(!Provider::OpenAi.caching() && !Provider::OpenAi.batch());
    }
}
