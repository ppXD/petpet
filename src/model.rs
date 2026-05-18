//! Model identifier normalization.
//!
//! Maps the raw `model` string a provider writes to its JSONL/SQLite into
//! a structured `ModelIdent` we can match XP rules against. The exact same
//! model called via different tools (Claude Code vs OpenCode vs anywhere)
//! must normalize to the same `model` / `family` / `vendor` / `tier` so a
//! single rule of `{"model": "claude-opus-4-7"}` fires for all of them.
//!
//! Normalization rules (applied in order):
//! 1. Lowercase.
//! 2. Strip recognised vendor prefix: `anthropic/`, `openai/`, `google/`,
//!    `openrouter/`, `meta/`, `alibaba/`, `deepseek/`, `huggingface/`.
//! 3. Strip trailing snapshot date: `-YYYYMMDD` (8+ digits at end).
//! 4. Replace `.` with `-` (Anthropic uses both `claude-opus-4-6` and
//!    `claude-opus-4.6` for the same model; we collapse to one).
//! 5. Insert a `-` between an alphabetic char and a digit IF not already
//!    separated — handles `qwen3.6` → `qwen-3-6` without breaking `gpt-4o`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Vendor {
    Anthropic,
    OpenAI,
    Google,
    Meta,
    Alibaba,
    DeepSeek,
    Unknown,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Anthropic => "anthropic",
            Vendor::OpenAI => "openai",
            Vendor::Google => "google",
            Vendor::Meta => "meta",
            Vendor::Alibaba => "alibaba",
            Vendor::DeepSeek => "deepseek",
            Vendor::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Frontier,
    Mid,
    Mini,
    Unknown,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Frontier => "frontier",
            Tier::Mid => "mid",
            Tier::Mini => "mini",
            Tier::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdent {
    /// Original string for forensic queries. Never used for matching.
    pub raw: String,
    /// Canonical normalized id, e.g. `"claude-opus-4-7"`, `"gpt-5-3-codex"`.
    pub model: String,
    /// Family group, e.g. `"claude-opus"`, `"gpt-5"`, `"o1"`.
    pub family: String,
    pub vendor: Vendor,
    pub tier: Tier,
}

impl ModelIdent {
    pub fn parse(raw: &str) -> Self {
        let raw_owned = raw.to_string();
        let normalized = normalize(raw);
        let family = identify_family(&normalized);
        let vendor = identify_vendor(&family, &normalized);
        let tier = identify_tier(&normalized, &family);
        Self {
            raw: raw_owned,
            model: normalized,
            family,
            vendor,
            tier,
        }
    }
}

fn normalize(raw: &str) -> String {
    let mut s = raw.trim().to_ascii_lowercase();

    // 1. Strip recognised vendor prefix (e.g. "anthropic/claude-opus-4-7")
    if let Some(idx) = s.find('/') {
        let prefix = &s[..idx];
        const KNOWN_PREFIXES: &[&str] = &[
            "anthropic",
            "openai",
            "google",
            "openrouter",
            "meta",
            "alibaba",
            "deepseek",
            "huggingface",
            "opencode",
            "ollama",
        ];
        if KNOWN_PREFIXES.iter().any(|p| *p == prefix) {
            s = s[idx + 1..].to_string();
        }
    }

    // 2. Strip trailing snapshot date suffix (-YYYYMMDD or longer all-digit run)
    if let Some(idx) = s.rfind('-') {
        let after = &s[idx + 1..];
        if after.len() >= 8 && after.chars().all(|c| c.is_ascii_digit()) {
            s = s[..idx].to_string();
        }
    }

    // 3. Replace dots with dashes
    s.replace('.', "-")

    // Note: we deliberately do NOT auto-insert dashes between letters and
    // digits. That breaks valid atom names like "o1", "v4", "gpt-4o", whose
    // digit is part of the family token. We rely on the family table below
    // to enumerate both `qwen3` and `qwen-3` style variants.
}

fn identify_family(normalized: &str) -> String {
    // Longest-match-wins family table. Each entry must match either the
    // full string or a prefix followed by '-'.
    const FAMILIES: &[&str] = &[
        // Anthropic
        "claude-opus", "claude-sonnet", "claude-haiku", "claude",
        // OpenAI
        "gpt-5", "gpt-4o", "gpt-4", "gpt-3-5", "gpt",
        "chatgpt",
        "o4", "o3", "o1",
        // Google
        "gemini-2", "gemini-1-5", "gemini-1", "gemini",
        // Meta
        "llama-3", "llama-2", "llama",
        // Alibaba — accept both `qwen-3` and `qwen3` style
        "qwen-3", "qwen-2-5", "qwen-2", "qwen3", "qwen2-5", "qwen2", "qwen",
        // DeepSeek
        "deepseek-v4", "deepseek-v3", "deepseek-v2", "deepseek-r1", "deepseek",
    ];

    // Pick longest match; ties broken by table order.
    let mut best: Option<&str> = None;
    for fam in FAMILIES {
        if normalized == *fam {
            return (*fam).to_string();
        }
        let prefix_with_dash = format!("{fam}-");
        if normalized.starts_with(&prefix_with_dash)
            && best.map(|b| b.len() < fam.len()).unwrap_or(true)
        {
            best = Some(fam);
        }
    }
    if let Some(b) = best {
        return b.to_string();
    }

    // Fallback: first dash-segment
    normalized
        .split('-')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

fn identify_vendor(family: &str, normalized: &str) -> Vendor {
    if family.starts_with("claude") {
        Vendor::Anthropic
    } else if family.starts_with("gpt")
        || family.starts_with("chatgpt")
        || family.starts_with("o1")
        || family.starts_with("o3")
        || family.starts_with("o4")
    {
        Vendor::OpenAI
    } else if family.starts_with("gemini") {
        Vendor::Google
    } else if family.starts_with("llama") {
        Vendor::Meta
    } else if family.starts_with("qwen") {
        Vendor::Alibaba
    } else if family.starts_with("deepseek") {
        Vendor::DeepSeek
    } else if normalized.starts_with("synthetic") || family == "synthetic" {
        Vendor::Unknown
    } else {
        Vendor::Unknown
    }
}

fn identify_tier(normalized: &str, family: &str) -> Tier {
    // Mini takes precedence. Use dash-segments as word boundaries so
    // "gemini" doesn't get caught by the "mini" substring scan.
    let segs: Vec<&str> = normalized.split('-').collect();
    if segs.iter().any(|s| matches!(*s, "mini" | "nano"))
        || normalized.contains("flash-lite")
        || family == "claude-haiku"
    {
        return Tier::Mini;
    }

    // Frontier: top-tier reasoning / flagship models
    if family == "claude-opus"
        || family.starts_with("claude-opus")
        || family == "gpt-5"
        || family.starts_with("o1")
        || family.starts_with("o3")
        || family.starts_with("o4")
    {
        return Tier::Frontier;
    }

    // Mid: balanced / previous-gen flagships
    if family.starts_with("claude-sonnet")
        || family.starts_with("gpt-4")
        || family.starts_with("gemini")
    {
        return Tier::Mid;
    }

    Tier::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> ModelIdent {
        ModelIdent::parse(s)
    }

    #[test]
    fn claude_opus_versions_distinct() {
        let m47 = parse("claude-opus-4-7");
        assert_eq!(m47.model, "claude-opus-4-7");
        assert_eq!(m47.family, "claude-opus");
        assert_eq!(m47.vendor, Vendor::Anthropic);
        assert_eq!(m47.tier, Tier::Frontier);

        let m46 = parse("claude-opus-4-6");
        assert_eq!(m46.model, "claude-opus-4-6");
        assert_ne!(m46.model, m47.model, "4.6 and 4.7 must be different");
    }

    #[test]
    fn dot_dash_normalize_collapses() {
        let dot = parse("claude-opus-4.6");
        let dash = parse("claude-opus-4-6");
        assert_eq!(dot.model, dash.model, "4.6 and 4-6 are the same model");
    }

    #[test]
    fn snapshot_date_stripped() {
        let snap = parse("claude-haiku-4-5-20251001");
        assert_eq!(snap.model, "claude-haiku-4-5");
        assert_eq!(snap.family, "claude-haiku");
        assert_eq!(snap.tier, Tier::Mini);
    }

    #[test]
    fn vendor_prefix_stripped() {
        let m = parse("anthropic/claude-opus-4-7");
        assert_eq!(m.model, "claude-opus-4-7");
    }

    #[test]
    fn sonnet_family_versions_distinct() {
        let s4 = parse("claude-sonnet-4");
        let s45 = parse("claude-sonnet-4-5-20250929");
        let s46 = parse("claude-sonnet-4-6");
        assert_eq!(s4.model, "claude-sonnet-4");
        assert_eq!(s45.model, "claude-sonnet-4-5");
        assert_eq!(s46.model, "claude-sonnet-4-6");
        assert_eq!(s45.family, "claude-sonnet");
        assert_eq!(s4.tier, Tier::Mid);
        assert_eq!(s45.tier, Tier::Mid);
    }

    #[test]
    fn haiku_is_mini() {
        let m = parse("claude-haiku-4-5-20251001");
        assert_eq!(m.tier, Tier::Mini);
    }

    #[test]
    fn gpt5_variants() {
        let codex = parse("gpt-5.3-codex");
        assert_eq!(codex.model, "gpt-5-3-codex");
        assert_eq!(codex.family, "gpt-5");
        assert_eq!(codex.tier, Tier::Frontier);

        let v54 = parse("gpt-5.4");
        assert_eq!(v54.model, "gpt-5-4");
        assert_eq!(v54.family, "gpt-5");
        assert_eq!(v54.vendor, Vendor::OpenAI);

        let v55 = parse("gpt-5.5");
        assert_eq!(v55.model, "gpt-5-5");
        assert_ne!(v55.model, v54.model);
    }

    #[test]
    fn gpt4o_mini_is_mini() {
        let m = parse("gpt-4o-mini");
        assert_eq!(m.tier, Tier::Mini);
        assert_eq!(m.family, "gpt-4o");
    }

    #[test]
    fn o_series_frontier() {
        assert_eq!(parse("o1").tier, Tier::Frontier);
        assert_eq!(parse("o3").tier, Tier::Frontier);
        assert_eq!(parse("o1-mini").tier, Tier::Mini);
    }

    #[test]
    fn qwen_compact_form_handled() {
        let m = parse("qwen3.6-plus-free");
        // Dots → dashes only; we don't auto-insert dashes between letter+digit
        // so the canonical form preserves the "qwen3" atom.
        assert_eq!(m.model, "qwen3-6-plus-free");
        assert_eq!(m.family, "qwen3");
        assert_eq!(m.vendor, Vendor::Alibaba);
    }

    #[test]
    fn deepseek_recognised() {
        let m = parse("deepseek-v4-flash-free");
        assert_eq!(m.vendor, Vendor::DeepSeek);
        // flash-lite would be mini but plain "flash" isn't; default tier
        assert_eq!(m.family, "deepseek-v4");
    }

    #[test]
    fn unknown_falls_back_safely() {
        let m = parse("some-random-model-99");
        assert_eq!(m.vendor, Vendor::Unknown);
        assert_eq!(m.tier, Tier::Unknown);
    }

    #[test]
    fn empty_string_does_not_panic() {
        let m = parse("");
        assert_eq!(m.vendor, Vendor::Unknown);
    }

    /// Fixture: every distinct model string we've actually observed on
    /// the user's machine — guards against regressions when normalize
    /// rules change.
    #[test]
    fn observed_models_normalize_stably() {
        let observed = [
            // From this user's DB (May 2026)
            ("claude-opus-4-7",        "claude-opus-4-7",  "claude-opus",   Vendor::Anthropic, Tier::Frontier),
            ("deepseek-v4-flash-free", "deepseek-v4-flash-free", "deepseek-v4", Vendor::DeepSeek, Tier::Unknown),
            ("gpt-5.5",                "gpt-5-5",          "gpt-5",         Vendor::OpenAI,    Tier::Frontier),
            ("qwen3.6-plus-free",      "qwen3-6-plus-free", "qwen3",       Vendor::Alibaba,   Tier::Unknown),
            // Other plausible models the parser MUST handle
            ("claude-haiku-4-5-20251001", "claude-haiku-4-5", "claude-haiku", Vendor::Anthropic, Tier::Mini),
            ("claude-opus-4-6",        "claude-opus-4-6",  "claude-opus",   Vendor::Anthropic, Tier::Frontier),
            ("claude-opus-4.6",        "claude-opus-4-6",  "claude-opus",   Vendor::Anthropic, Tier::Frontier),
            ("claude-sonnet-4",        "claude-sonnet-4",  "claude-sonnet", Vendor::Anthropic, Tier::Mid),
            ("claude-sonnet-4-6",      "claude-sonnet-4-6","claude-sonnet", Vendor::Anthropic, Tier::Mid),
            ("gpt-5.3-codex",          "gpt-5-3-codex",    "gpt-5",         Vendor::OpenAI,    Tier::Frontier),
            ("gpt-5.4",                "gpt-5-4",          "gpt-5",         Vendor::OpenAI,    Tier::Frontier),
            ("gpt-4o",                 "gpt-4o",           "gpt-4o",        Vendor::OpenAI,    Tier::Mid),
            ("gpt-4o-mini",            "gpt-4o-mini",      "gpt-4o",        Vendor::OpenAI,    Tier::Mini),
            ("o1",                     "o1",               "o1",            Vendor::OpenAI,    Tier::Frontier),
            ("o3",                     "o3",               "o3",            Vendor::OpenAI,    Tier::Frontier),
            ("o1-mini",                "o1-mini",          "o1",            Vendor::OpenAI,    Tier::Mini),
            ("gemini-2-0-flash",       "gemini-2-0-flash", "gemini-2",      Vendor::Google,    Tier::Mid),
            ("anthropic/claude-opus-4-7", "claude-opus-4-7","claude-opus",  Vendor::Anthropic, Tier::Frontier),
            ("openai/gpt-5.5",         "gpt-5-5",          "gpt-5",         Vendor::OpenAI,    Tier::Frontier),
        ];
        for (raw, want_model, want_family, want_vendor, want_tier) in observed {
            let m = parse(raw);
            assert_eq!(m.model,  want_model,  "model for {raw}");
            assert_eq!(m.family, want_family, "family for {raw}");
            assert_eq!(m.vendor, want_vendor, "vendor for {raw}");
            assert_eq!(m.tier,   want_tier,   "tier for {raw}");
        }
    }
}
