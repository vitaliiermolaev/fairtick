//! Nickname policy — the SINGLE source of truth for what a nickname may be and how a
//! legacy/raw value is canonicalized into stored shape.
//!
//! Two callers, deliberately sharing one ruleset:
//!   * auth (the live write path) calls [`validate`] on every set/change, so a stored
//!     nickname is always trimmed, length-bounded, and free of control / zero-width / bidi
//!     characters.
//!   * db (migration 005) calls [`canonicalize_legacy`] to coerce pre-uniqueness rows into
//!     that SAME shape BEFORE building the case-insensitive UNIQUE index — otherwise the
//!     index would still admit visually-identical twins the live validator would reject
//!     (" Bob " vs "Bob", "Bob\u{200B}" vs "Bob"). Migration can't *reject* a row, so it
//!     coerces instead of erroring.
//!
//! Keeping both in one module is what guarantees "a name migration keeps is a name the live
//! validator would also accept."

/// Max nickname length, counted in Unicode scalar values (Rust `char`), AFTER trimming.
/// The Unity client mirrors this as its own constant (no shared schema yet — the value is
/// small and stable; revisit with a generated config if more rules accrue).
pub const MAX_NICKNAME_CHARS: usize = 30;

/// Control + zero-width / bidi-format codepoints. Rejected (write path) or stripped
/// (migration): newlines are a log-injection vector; zero-width/joiner chars are invisible
/// whitespace impersonation; RTL overrides + isolates are display-spoofing. Ordinary spaces
/// between words are fine — `trim` handles the edges.
pub fn is_forbidden_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'   // zero-width space/joiners + LRM/RLM
            | '\u{202A}'..='\u{202E}' // bidi embeddings / overrides
            | '\u{2060}'..='\u{2064}' // word-joiner + invisible math operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}') // BOM / zero-width no-break space
}

/// Validate + canonicalize a nickname for STORAGE. Returns the value to store (trimmed,
/// internal spaces kept). `Err` is a stable, client-safe reason for the 400 body — the two
/// variants are distinct on purpose so "unsupported characters" isn't reported as a length
/// problem.
pub fn validate(nickname: &str) -> Result<String, &'static str> {
    let trimmed = nickname.trim();
    let len = trimmed.chars().count();
    if len == 0 || len > MAX_NICKNAME_CHARS {
        return Err("Nickname must be 1-30 characters");
    }
    if trimmed.chars().any(is_forbidden_char) {
        return Err("Nickname has unsupported characters");
    }
    Ok(trimmed.to_string())
}

/// Coerce an ARBITRARY legacy value into stored shape WITHOUT rejecting it (a migration
/// can't fail a row): trim, drop forbidden chars, truncate to [`MAX_NICKNAME_CHARS`] scalar
/// values, re-trim the edges, and fall back to a placeholder if nothing usable remains. The
/// result always satisfies [`validate`].
pub fn canonicalize_legacy(nickname: &str) -> String {
    let cleaned: String =
        nickname.trim().chars().filter(|c| !is_forbidden_char(*c)).take(MAX_NICKNAME_CHARS).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "player".to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trims_keeps_internal_spaces_and_bounds_length() {
        assert_eq!(validate("  Sly Fox  ").unwrap(), "Sly Fox");
        for bad in ["", "   ", &"x".repeat(MAX_NICKNAME_CHARS + 1)] {
            assert_eq!(validate(bad), Err("Nickname must be 1-30 characters"));
        }
    }

    #[test]
    fn validate_rejects_control_and_format_chars_distinctly() {
        for bad in ["evil\nadmin", "ab\tcd", "ze\u{200B}ro", "rtl\u{202E}flip", "bom\u{FEFF}"] {
            assert_eq!(validate(bad), Err("Nickname has unsupported characters"), "should reject {bad:?}");
        }
    }

    #[test]
    fn canonicalize_legacy_matches_validate_shape() {
        // Padding/format chars that the index would otherwise treat as distinct names.
        assert_eq!(canonicalize_legacy(" Bob "), "Bob");
        assert_eq!(canonicalize_legacy("Bob\u{200B}"), "Bob");
        assert_eq!(canonicalize_legacy("Bob\nadmin"), "Bobadmin");
        // Whatever comes out the other side must be a value the live validator accepts.
        for raw in [" Bob ", "Bob\u{200B}", "Bob\nadmin", "", "   ", "\u{202E}\u{200B}", &"y".repeat(40)] {
            let c = canonicalize_legacy(raw);
            assert!(validate(&c).is_ok(), "canonicalized {raw:?} -> {c:?} must validate");
        }
        // Nothing usable left → placeholder, still valid.
        assert_eq!(canonicalize_legacy("\u{200B}\u{202E}"), "player");
    }
}
