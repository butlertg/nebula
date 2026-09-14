//! The Codex MODEL list.
//!
//! `codex --model` takes a slug (`gpt-5.6-terra`) and the set of slugs an
//! account may use is the API's to decide, not this build's: codex refetches
//! its own list into `~/.codex/models_cache.json`, and a model released after
//! a nebula release cannot be picked from a const that shipped before it.
//!
//! So the list resolves from two sources, first non-empty wins:
//!
//! 1. `codex_models` in CONFIG.JSON — the user's own list, verbatim, synced
//!    on every [`crate::config::Config::load`] so a hand edit applies
//!    without a restart.
//! 2. [`CODEX_MODELS`], the built-in slugs.
//!
//! This is the Codex half of what `claude_catalogue.rs` does for Claude, and
//! deliberately the simpler half: Codex has no `availableModels` allowlist to
//! read, so CONFIG.JSON is the only source that can override the built-ins.
//!
//! [`DEFAULT_CHOICE`] always heads the list — "pass no flag" is right under
//! any account — and lists are handed out as `&'static` slices, the shape
//! every MODEL / EFFORT surface takes, by leaking a new one only when the
//! resolved list actually changes.

use std::sync::{OnceLock, RwLock};

use crate::config::{CODEX_MODELS, DEFAULT_CHOICE};

/// The choice list for the given source: [`DEFAULT_CHOICE`] first, then
/// `configured` (CONFIG.JSON) trimmed and deduplicated case-insensitively;
/// the built-in slugs when it is empty.
pub fn resolve(configured: &[String]) -> Vec<String> {
    let mut out = vec![DEFAULT_CHOICE.to_string()];
    for model in configured {
        let model = model.trim();
        if model.is_empty() || out.iter().any(|m| m.eq_ignore_ascii_case(model)) {
            continue;
        }
        out.push(model.to_string());
    }
    // A list of nothing but blanks (or only "default") is no list.
    if out.len() > 1 {
        out
    } else {
        CODEX_MODELS.iter().map(|m| m.to_string()).collect()
    }
}

struct State {
    configured: Vec<String>,
    installed: &'static [&'static str],
}

static CURRENT: OnceLock<RwLock<State>> = OnceLock::new();

fn state() -> &'static RwLock<State> {
    CURRENT.get_or_init(|| {
        RwLock::new(State {
            configured: Vec::new(),
            installed: CODEX_MODELS,
        })
    })
}

/// The MODEL choices for Codex, as the pickers, the AGENTS TAB and the
/// PRESET EDITOR list them.
pub fn models() -> &'static [&'static str] {
    state()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .installed
}

fn leak(list: &[String]) -> &'static [&'static str] {
    if list
        .iter()
        .map(String::as_str)
        .eq(CODEX_MODELS.iter().copied())
    {
        return CODEX_MODELS;
    }
    let leaked: Vec<&'static str> = list
        .iter()
        .map(|m| {
            if m == DEFAULT_CHOICE {
                DEFAULT_CHOICE
            } else {
                Box::leak(m.clone().into_boxed_str()) as &'static str
            }
        })
        .collect();
    Box::leak(leaked.into_boxed_slice())
}

/// Adopt CONFIG.JSON's `codex_models`. Called from every `Config::load`
/// outside tests, so it is cheap when nothing changed and leaks only when
/// the list did. Not called from tests: the view is process-global.
pub fn sync_config(configured: &[String]) {
    let mut state = state()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.configured == configured {
        return;
    }
    state.configured = configured.to_vec();
    let list = resolve(&state.configured);
    if list
        .iter()
        .map(String::as_str)
        .eq(state.installed.iter().copied())
    {
        return;
    }
    state.installed = leak(&list);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_falls_back_to_the_built_in_slugs() {
        assert_eq!(resolve(&[]), CODEX_MODELS);
        assert_eq!(resolve(&["".into(), "  ".into()]), CODEX_MODELS);
        // "default" alone is the sentinel, not a list.
        assert_eq!(resolve(&[DEFAULT_CHOICE.into()]), CODEX_MODELS);
    }

    #[test]
    fn a_configured_list_replaces_the_built_ins_with_default_first() {
        assert_eq!(
            resolve(&["gpt-5.6-terra".into(), "gpt-6-astra".into()]),
            vec![DEFAULT_CHOICE, "gpt-5.6-terra", "gpt-6-astra"],
        );
        // Naming the sentinel does not duplicate it or move it.
        assert_eq!(
            resolve(&["gpt-6-astra".into(), DEFAULT_CHOICE.into()]),
            vec![DEFAULT_CHOICE, "gpt-6-astra"],
        );
    }

    #[test]
    fn blanks_and_case_insensitive_duplicates_drop_out() {
        assert_eq!(
            resolve(&[
                " gpt-6-astra ".into(),
                "".into(),
                "GPT-6-ASTRA".into(),
                "gpt-5.5".into(),
            ]),
            vec![DEFAULT_CHOICE, "gpt-6-astra", "gpt-5.5"],
        );
    }

    /// A list that matches the built-ins comes back as the built-ins.
    /// (Identity is not assertable: `CODEX_MODELS` is a const, so every use
    /// site may hold its own copy — the early return still avoids the leak.)
    #[test]
    fn an_equivalent_list_comes_back_as_the_built_ins() {
        let same: Vec<String> = CODEX_MODELS.iter().map(|m| m.to_string()).collect();
        assert_eq!(leak(&same), CODEX_MODELS);
    }
}
