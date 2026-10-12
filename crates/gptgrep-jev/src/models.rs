//! Decisions model identity: exact selectors, observed canonical builds and
//! OpenRouter `~author/family-latest` aliases.
//!
//! OpenRouter marks an alias with a leading `~`. The catalog entry for an alias
//! carries `alias_target.slug`, the pinned model it currently resolves to, and the
//! response `model` field returns the dated canonical build that actually answered.
//! The only alias convention admitted here is `~author/family-latest`, which the
//! catalog describes as always redirecting to the latest model of that family.
//! A response to an alias is accepted only when the served identifier is a
//! concrete build of the same author and family: `author/family-VERSION`, where
//! VERSION starts with an ASCII digit (for example `typesafe/jev-1.13` or
//! `typesafe/jev-1.13-20260917` for `~typesafe/jev-latest`). Another author,
//! another family (`typesafe/jevx-2`, `typesafe/jev-mini-1`), a variant suffix
//! (`:free`) or another alias is never admitted. Non-alias selectors keep exact
//! equality plus the two pinned canonical mappings.

use anyhow::{Result, ensure};

use crate::{DEFAULT_MODEL, LEGACY_MODEL};

/// Leading marker of an OpenRouter model alias.
pub const MODEL_ALIAS_PREFIX: char = '~';
const ALIAS_SUFFIX: &str = "-latest";
const MAX_MODEL_BYTES: usize = 256;
const MAX_SEGMENT_BYTES: usize = 128;
/// Observed canonical build for the default Luna Decisions selector.
pub const DEFAULT_MODEL_CANONICAL: &str = "openai/gpt-6-luna-decisions-20261006";
/// Observed canonical build for the explicit legacy Jev selector.
pub const LEGACY_MODEL_CANONICAL: &str = "typesafe/jev-1.13-20260917";

/// A syntactically valid `~author/family-latest` alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelAlias<'a> {
    author: &'a str,
    family: &'a str,
}

impl<'a> ModelAlias<'a> {
    /// `None` for a non-alias identifier. `Some(Err)` for a `~` identifier that
    /// does not follow the admitted `~author/family-latest` convention.
    pub fn parse(model: &'a str) -> Option<Result<Self>> {
        let rest = model.strip_prefix(MODEL_ALIAS_PREFIX)?;
        Some(Self::parse_rest(model, rest))
    }

    fn parse_rest(model: &'a str, rest: &'a str) -> Result<Self> {
        let (author, name) = rest
            .split_once('/')
            .filter(|_| model.len() <= MAX_MODEL_BYTES)
            .unwrap_or_default();
        let family = name.strip_suffix(ALIAS_SUFFIX).unwrap_or_default();
        ensure!(
            slug_segment(author) && slug_segment(family) && !family.ends_with(ALIAS_SUFFIX),
            "Unsupported Decisions model alias; only ~author/family-latest is admitted and no request was prepared or submitted"
        );
        Ok(Self { author, family })
    }

    pub fn author(&self) -> &'a str {
        self.author
    }

    pub fn family(&self) -> &'a str {
        self.family
    }

    /// True only for a concrete `author/family-VERSION` build of this family.
    /// VERSION starts with an ASCII digit and uses lowercase slug characters.
    pub fn admits(&self, served: &str) -> bool {
        if served.len() > MAX_MODEL_BYTES {
            return false;
        }
        let Some((author, name)) = served.split_once('/') else {
            return false;
        };
        let Some(version) = name
            .strip_prefix(self.family)
            .and_then(|rest| rest.strip_prefix('-'))
        else {
            return false;
        };
        author == self.author
            && version.as_bytes().first().is_some_and(u8::is_ascii_digit)
            && slug_segment(version)
    }
}

/// A lowercase OpenRouter slug segment: ASCII letters, digits, `.`, `_` and `-`,
/// starting and ending with a letter or digit. No `/`, `:`, `~` or whitespace.
fn slug_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SEGMENT_BYTES
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
}

/// True when the identifier uses OpenRouter's leading-`~` alias marker.
pub fn is_model_alias(model: &str) -> bool {
    model.starts_with(MODEL_ALIAS_PREFIX)
}

/// Admit the served model for one requested selector.
///
/// Exact identifiers require equality, except the two pinned canonical builds
/// for the default Luna and legacy Jev selectors. A `~author/family-latest`
/// alias admits only a concrete build of the same author and family; a
/// malformed alias admits nothing.
pub fn response_model_matches(requested: &str, actual: &str) -> bool {
    match ModelAlias::parse(requested) {
        Some(Ok(alias)) => alias.admits(actual),
        Some(Err(_)) => false,
        None => {
            requested == actual
                || matches!(
                    (requested, actual),
                    (DEFAULT_MODEL, DEFAULT_MODEL_CANONICAL)
                        | (LEGACY_MODEL, LEGACY_MODEL_CANONICAL)
                )
        }
    }
}

/// Exact Luna Decisions identities that may carry typed image evidence.
pub(crate) fn luna_decisions_model(model: &str) -> bool {
    matches!(model, DEFAULT_MODEL | DEFAULT_MODEL_CANONICAL)
}

/// Whether typed image evidence may be prepared for this requested selector.
///
/// Only the exact Luna Decisions selector and its pinned canonical build accept
/// images. Every alias is text-only: an alias may resolve to a later build that
/// is outside the pinned Luna identity set, so it is never provably Luna before
/// the paid response. Images for other selectors are rejected before any
/// reservation or submission.
pub fn model_accepts_images(model: &str) -> bool {
    luna_decisions_model(model)
}

/// Validate a requested selector before any client, reservation or request exists.
pub(crate) fn validate_requested_model(model: &str) -> Result<()> {
    ensure!(
        !model.trim().is_empty()
            && model.len() <= MAX_MODEL_BYTES
            && !model.chars().any(char::is_control),
        "Invalid Jev model identifier"
    );
    if let Some(alias) = ModelAlias::parse(model) {
        alias?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_grammar_is_author_family_latest_only() {
        let alias = ModelAlias::parse("~typesafe/jev-latest").unwrap().unwrap();
        assert_eq!((alias.author(), alias.family()), ("typesafe", "jev"));
        let luna = ModelAlias::parse("~openai/gpt-6-luna-decisions-latest")
            .unwrap()
            .unwrap();
        assert_eq!(luna.family(), "gpt-6-luna-decisions");
        assert!(ModelAlias::parse("typesafe/jev-1.13").is_none());
        assert!(ModelAlias::parse(DEFAULT_MODEL).is_none());
        for malformed in [
            "~",
            "~typesafe",
            "~typesafe/",
            "~/jev-latest",
            "~typesafe/jev",
            "~typesafe/-latest",
            "~typesafe/latest",
            "~typesafe/jev-latest-latest",
            "~TypeSafe/jev-latest",
            "~typesafe/Jev-latest",
            "~typesafe/jev latest",
            "~typesafe/jev:free-latest",
            "~typesafe/sub/jev-latest",
            "~~typesafe/jev-latest",
            "~typesafe/jev-latest:free",
            "~-typesafe/jev-latest",
            "~typesafe/jev.-latest",
        ] {
            assert!(
                ModelAlias::parse(malformed).unwrap().is_err(),
                "{malformed} must be rejected"
            );
            assert!(validate_requested_model(malformed).is_err());
            assert!(!response_model_matches(malformed, malformed));
            assert!(!response_model_matches(malformed, "typesafe/jev-1.13"));
        }
        let long = format!("~typesafe/{}-latest", "j".repeat(300));
        assert!(ModelAlias::parse(&long).unwrap().is_err());
    }

    #[test]
    fn alias_admits_only_same_author_same_family_concrete_builds() {
        let requested = "~typesafe/jev-latest";
        for served in [
            "typesafe/jev-1.13",
            "typesafe/jev-1.13-20260917",
            "typesafe/jev-2",
            "typesafe/jev-2.0-preview",
            "typesafe/jev-10.1_rc1",
        ] {
            assert!(
                response_model_matches(requested, served),
                "{served} must be admitted"
            );
        }
        for served in [
            // Another author or family, including prefix and suffix confusions.
            "other/jev-1.13",
            "typesafe-labs/jev-1.13",
            "openai/gpt-6-luna-decisions-20261006",
            "typesafe/jevx-1.13",
            "typesafe/jev-mini-1.0",
            "typesafe/xjev-1.13",
            "typesafe/jev",
            "typesafe/jev-",
            "typesafe/jev-latest",
            "typesafe/jev-v1.13",
            // Variants, aliases, case, nesting and malformed versions.
            "typesafe/jev-1.13:free",
            "~typesafe/jev-latest",
            "~typesafe/jev-1.13",
            "TypeSafe/jev-1.13",
            "typesafe/Jev-1.13",
            "typesafe/jev-1.13/extra",
            "typesafe/jev-1.13-",
            "typesafe/jev-1.13 ",
            " typesafe/jev-1.13",
            "typesafe/jev-1.13\n",
            "",
            "typesafe",
        ] {
            assert!(
                !response_model_matches(requested, served),
                "{served:?} must be rejected"
            );
        }
        let oversized = format!("typesafe/jev-1{}", "0".repeat(300));
        assert!(!response_model_matches(requested, &oversized));
        let luna = "~openai/gpt-6-luna-decisions-latest";
        assert!(response_model_matches(
            luna,
            "openai/gpt-6-luna-decisions-20261006"
        ));
        assert!(!response_model_matches(luna, "openai/gpt-6-luna-decisions"));
        assert!(!response_model_matches(luna, "typesafe/jev-1.13-20260917"));
    }

    #[test]
    fn exact_selectors_keep_equality_and_pinned_canonical_builds_only() {
        assert!(response_model_matches(DEFAULT_MODEL, DEFAULT_MODEL));
        assert!(response_model_matches(
            DEFAULT_MODEL,
            DEFAULT_MODEL_CANONICAL
        ));
        assert!(!response_model_matches(
            DEFAULT_MODEL,
            "openai/gpt-6-luna-decisions-20261007"
        ));
        assert!(response_model_matches(LEGACY_MODEL, LEGACY_MODEL));
        assert!(response_model_matches(LEGACY_MODEL, LEGACY_MODEL_CANONICAL));
        // Exact selectors never gain family admission from the alias rule.
        assert!(!response_model_matches(
            LEGACY_MODEL,
            "typesafe/jev-1.13-20260918"
        ));
        assert!(!response_model_matches(LEGACY_MODEL, "typesafe/jev-2"));
        assert!(response_model_matches("explicit/other", "explicit/other"));
        assert!(!response_model_matches(
            "explicit/other",
            "explicit/other-dated"
        ));
        assert!(!response_model_matches(
            DEFAULT_MODEL_CANONICAL,
            DEFAULT_MODEL
        ));
    }

    #[test]
    fn images_stay_with_exact_luna_identities_and_aliases_are_text_only() {
        assert!(model_accepts_images(DEFAULT_MODEL));
        assert!(model_accepts_images(DEFAULT_MODEL_CANONICAL));
        for model in [
            LEGACY_MODEL,
            LEGACY_MODEL_CANONICAL,
            "~typesafe/jev-latest",
            "~openai/gpt-6-luna-decisions-latest",
            "openai/gpt-6-luna-decisions-20261007",
            "explicit/other",
        ] {
            assert!(!model_accepts_images(model), "{model} must be text-only");
        }
        assert!(is_model_alias("~typesafe/jev-latest"));
        assert!(!is_model_alias(LEGACY_MODEL));
        assert!(validate_requested_model("~typesafe/jev-latest").is_ok());
        assert!(validate_requested_model("explicit/other").is_ok());
        assert!(validate_requested_model(" ").is_err());
        assert!(validate_requested_model("bad\nmodel").is_err());
    }
}
