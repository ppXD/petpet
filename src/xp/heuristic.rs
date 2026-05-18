//! Tier-classification heuristic + anti-cheat input validation.
//!
//! When a model name flows through that the registry hasn't seen yet
//! (Day-0 vendor release, mirror endpoint, fork, etc.), we still need
//! to classify it into a tier so the pet keeps growing. The heuristic
//! here is intentionally simple and substring-based — it never returns
//! `Tier::Unknown`, only one of `Mini / Mid / Frontier`, accompanied by
//! a confidence signal that the caller uses to dampen the XP.
//!
//! # Why heuristic, not "default to zero"
//!
//! UX: pet stops growing on new models → user thinks petpet is broken
//! before they think "oh, that model is too new." Heuristic-guessed
//! tier × `Confidence::Heuristic` (0.7×) preserves growth at a slight
//! discount, with a UI badge so the user knows we're guessing.
//!
//! # Why discount on heuristic instead of giving full XP
//!
//! Mild anti-cheat: a heuristic that ALWAYS gives full XP is a path to
//! invent fake model names like `my-super-special-9000` and farm XP.
//! The 0.7× / 0.4× discount makes that strictly worse than using a
//! registered model, so the rational play is to wait for registry
//! support — which makes our incentives align with growing the registry.
//!
//! # Validation (the anti-cheat layer)
//!
//! Separate from the heuristic: we reject implausible inputs before
//! they reach the scorer. Specifically:
//!
//! - Empty / oversized model names.
//! - Non-ASCII / non-lowercase characters (zero-width, RTL override,
//!   uppercase pretending to be a unique model).
//! - Token totals beyond any plausible context (5M+).
//!
//! Rejected inputs produce zero XP — same as an unknown event class.

use crate::event::TokenDelta;
use crate::model::Tier;

// ─── Heuristic tier classification ──────────────────────────────────

/// Result of the heuristic tier classifier.
///
/// `Confident` means at least one keyword matched, so the tier guess
/// is based on real signal (caller maps to `Confidence::Heuristic`).
///
/// `Default` means no signal — we fell back to Mid as a safe middle
/// (caller maps to `Confidence::Unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackResult {
    Confident(Tier),
    Default,
}

/// Mini-tier signals — appear as a dash-separated segment OR a known
/// multi-token phrase. Most cheap / small-parameter / instant-class
/// models match here.
const MINI_KEYWORDS: &[&str] = &[
    "nano", "mini", "haiku", "small", "lite", "tiny", "lightning",
];

/// Multi-token phrases (substring match, not segment match) for Mini.
const MINI_PHRASES: &[&str] = &["flash-lite", "8b-instruct"];

/// Parameter-size tags signalling Mini. ≤8B models.
const MINI_SIZE_TAGS: &[&str] = &["8b", "7b", "3b", "1-5b", "1b", "0-5b"];

/// Frontier-tier signals — flagship / largest / hardest-reasoning.
const FRONTIER_KEYWORDS: &[&str] = &[
    "opus", "ultra", "max", "frontier", "pro",
    // OpenAI o-series reasoning flagships
    "o1", "o3", "o4", "o5",
];

/// Parameter-size tags signalling Frontier. ≥70B parameters.
const FRONTIER_SIZE_TAGS: &[&str] = &["70b", "175b", "405b", "671b"];

/// Classify a normalized model name into a tier when the registry /
/// family table didn't recognise it. Never returns `Tier::Unknown`.
///
/// The `name` is expected to already be normalized (lowercase, dashes,
/// vendor prefix stripped). Pass it through `crate::model::ModelIdent::parse`
/// or call `normalize` yourself first.
///
/// Order matters: Mini takes precedence over Frontier when both keywords
/// appear (e.g. `claude-opus-mini-experimental` is Mini — the user
/// explicitly opted into the smaller variant). Within Mini and Frontier,
/// first match wins.
pub fn fallback_tier(name: &str) -> FallbackResult {
    let segs: Vec<&str> = name.split('-').collect();

    // Mini phrases (substring) → check first
    if MINI_PHRASES.iter().any(|p| name.contains(p)) {
        return FallbackResult::Confident(Tier::Mini);
    }

    // Mini keywords as segments
    if segs.iter().any(|s| MINI_KEYWORDS.contains(s)) {
        return FallbackResult::Confident(Tier::Mini);
    }

    // Mini size tags as segments
    if segs.iter().any(|s| MINI_SIZE_TAGS.contains(s)) {
        return FallbackResult::Confident(Tier::Mini);
    }

    // Frontier keywords as segments
    if segs.iter().any(|s| FRONTIER_KEYWORDS.contains(s)) {
        return FallbackResult::Confident(Tier::Frontier);
    }

    // Frontier size tags as segments
    if segs.iter().any(|s| FRONTIER_SIZE_TAGS.contains(s)) {
        return FallbackResult::Confident(Tier::Frontier);
    }

    // No signal → default Mid
    FallbackResult::Default
}

// ─── Input validation (anti-cheat) ──────────────────────────────────

/// Reject model names that can't be real. Validates the NORMALIZED form
/// (lowercase, no vendor prefix), so callers should run
/// `ModelIdent::parse` first and pass `.model`.
///
/// Accepted:  `[a-z0-9._/-]{1,80}`
/// Rejected:  empty, oversized, uppercase, unicode, weird chars
pub fn validate_model_name(normalized: &str) -> bool {
    if normalized.is_empty() || normalized.len() > 80 {
        return false;
    }
    normalized.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '-'
            || c == '_'
            || c == '.'
            || c == '/'
    })
}

/// Reject implausible token totals — a single event reporting more
/// than 5M tokens is almost certainly a hook-parsing bug or replay.
/// 5M comfortably exceeds the largest context window in production
/// (Gemini 2.5 Pro at 2M); a single TURN can't legitimately consume
/// more than its own context.
pub const MAX_TOKENS_PER_EVENT: u64 = 5_000_000;

pub fn validate_tokens(tokens: &TokenDelta) -> bool {
    tokens.total() <= MAX_TOKENS_PER_EVENT
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Heuristic classification ───────────────────────────────────

    #[test]
    fn opus_classified_frontier() {
        assert_eq!(
            fallback_tier("claude-opus-5-1"),
            FallbackResult::Confident(Tier::Frontier)
        );
    }

    #[test]
    fn haiku_classified_mini() {
        assert_eq!(
            fallback_tier("claude-haiku-9-2"),
            FallbackResult::Confident(Tier::Mini)
        );
    }

    #[test]
    fn nano_in_segment_classified_mini() {
        assert_eq!(
            fallback_tier("gpt-9-nano"),
            FallbackResult::Confident(Tier::Mini)
        );
    }

    #[test]
    fn mini_in_segment_classified_mini() {
        assert_eq!(
            fallback_tier("future-model-mini"),
            FallbackResult::Confident(Tier::Mini)
        );
    }

    #[test]
    fn flash_lite_phrase_classified_mini() {
        assert_eq!(
            fallback_tier("gemini-9-flash-lite"),
            FallbackResult::Confident(Tier::Mini)
        );
    }

    #[test]
    fn o_series_classified_frontier() {
        // o1, o3, o4 as standalone segments.
        assert_eq!(fallback_tier("o5"), FallbackResult::Confident(Tier::Frontier));
        assert_eq!(
            fallback_tier("openai-o5-preview"),
            FallbackResult::Confident(Tier::Frontier)
        );
    }

    #[test]
    fn mini_beats_frontier_when_both_present() {
        // `claude-opus-mini-experimental` — user opted into Mini despite
        // the Opus marker. We respect that.
        assert_eq!(
            fallback_tier("claude-opus-mini-experimental"),
            FallbackResult::Confident(Tier::Mini)
        );
    }

    #[test]
    fn size_tag_70b_frontier() {
        assert_eq!(
            fallback_tier("llama-5-70b-instruct"),
            FallbackResult::Confident(Tier::Frontier)
        );
    }

    #[test]
    fn size_tag_405b_frontier() {
        assert_eq!(
            fallback_tier("llama-5-405b"),
            FallbackResult::Confident(Tier::Frontier)
        );
    }

    #[test]
    fn size_tag_8b_mini() {
        assert_eq!(
            fallback_tier("llama-5-8b-instruct"),
            FallbackResult::Confident(Tier::Mini)
        );
    }

    #[test]
    fn no_signal_defaults_to_mid() {
        // Random future name with no recognised marker.
        assert_eq!(fallback_tier("zephyr-7000"), FallbackResult::Default);
        assert_eq!(fallback_tier("random-string"), FallbackResult::Default);
    }

    #[test]
    fn gemini_does_not_match_mini_substring() {
        // Defensive: `gemini` contains the substring `mini` but should
        // not classify as Mini (gemini-2.5-pro is Mid/Frontier-tier).
        // Heuristic uses SEGMENTS not substrings for `mini` keyword,
        // so `gemini` stays Default → caller treats as Mid.
        assert_eq!(fallback_tier("gemini-9-pro"), FallbackResult::Confident(Tier::Frontier));
        assert_eq!(fallback_tier("gemini-9"), FallbackResult::Default);
    }

    #[test]
    fn pro_segment_classified_frontier() {
        // `pro` segment alone is a Frontier signal (Gemini Pro, Qwen Pro).
        assert_eq!(
            fallback_tier("future-model-pro"),
            FallbackResult::Confident(Tier::Frontier)
        );
    }

    // ─── Validation: model name ─────────────────────────────────────

    #[test]
    fn validate_accepts_canonical_names() {
        assert!(validate_model_name("claude-opus-4-7"));
        assert!(validate_model_name("gpt-5-mini"));
        assert!(validate_model_name("o3"));
        assert!(validate_model_name("gemini-2.5-pro"));
        assert!(validate_model_name("deepseek-v3"));
        assert!(validate_model_name("llama-3.3-70b-instruct"));
        assert!(validate_model_name("custom_model_v1"));
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(!validate_model_name(""));
    }

    #[test]
    fn validate_rejects_oversized() {
        let huge = "a".repeat(81);
        assert!(!validate_model_name(&huge));
        let max = "a".repeat(80);
        assert!(validate_model_name(&max));
    }

    #[test]
    fn validate_rejects_uppercase() {
        // Validation runs on the NORMALIZED form — uppercase indicates
        // normalization was skipped, which is itself a bug to flag.
        assert!(!validate_model_name("Claude-Opus-4-7"));
        assert!(!validate_model_name("GPT-5"));
    }

    #[test]
    fn validate_rejects_unicode_chars() {
        // Zero-width space, RTL override, emoji — all classic
        // "looks like a real model name" cheat attempts.
        assert!(!validate_model_name("claude\u{200B}opus"));
        assert!(!validate_model_name("claude\u{202E}opus"));
        assert!(!validate_model_name("opus🎉"));
    }

    #[test]
    fn validate_rejects_shell_metachars() {
        assert!(!validate_model_name("claude;rm-rf"));
        assert!(!validate_model_name("opus|cat"));
        assert!(!validate_model_name("model$INJECTION"));
        assert!(!validate_model_name("model with space"));
    }

    // ─── Validation: tokens ─────────────────────────────────────────

    #[test]
    fn validate_accepts_plausible_tokens() {
        assert!(validate_tokens(&TokenDelta::default()));
        assert!(validate_tokens(&TokenDelta {
            input: 100_000,
            output: 50_000,
            cache_read: 1_000_000,
            cache_creation: 10_000,
            reasoning: 30_000,
        }));
    }

    #[test]
    fn validate_rejects_implausible_totals() {
        // Exceeds 5M total.
        let bad = TokenDelta {
            input: 0,
            output: 0,
            cache_read: 6_000_000,
            cache_creation: 0,
            reasoning: 0,
        };
        assert!(!validate_tokens(&bad));
    }

    #[test]
    fn validate_accepts_exact_max() {
        let max = TokenDelta {
            input: MAX_TOKENS_PER_EVENT,
            output: 0,
            cache_read: 0,
            cache_creation: 0,
            reasoning: 0,
        };
        assert!(validate_tokens(&max));
    }

    #[test]
    fn max_tokens_per_event_pinned() {
        // Anchor: changing this changes the anti-cheat threshold.
        assert_eq!(MAX_TOKENS_PER_EVENT, 5_000_000);
    }
}
