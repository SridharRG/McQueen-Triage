//! Secret scrubbing for anything that leaves the box (LLM prompts, chat
//! notifications). Logs are stored unredacted locally; redaction happens at
//! the point context is assembled.

use std::sync::LazyLock;

use regex::Regex;

struct Rule {
    re: Regex,
    replacement: &'static str,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |pattern: &str, replacement: &'static str| Rule {
        re: Regex::new(pattern).expect("valid redaction regex"),
        replacement,
    };
    vec![
        // scheme://user:password@host
        rule(r"(?i)\b([a-z][a-z0-9+.-]*://[^:/\s@]+:)[^@\s/]+@", "${1}***@"),
        // Authorization headers / bearer tokens
        rule(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}", "$1 ***"),
        // JWTs
        rule(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", "<jwt>"),
        // Provider keys
        rule(r"\b(gsk|sk|pk|rk|xox[abpr])[-_][A-Za-z0-9_-]{16,}", "<api-key>"),
        rule(r"\bAKIA[0-9A-Z]{16}\b", "<aws-access-key>"),
        // "password": "…", password=…, secret/token/api_key style keys (JSON or k=v)
        rule(
            r#"(?i)("?(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|secret[_-]?key|session|cookie|authorization|x-api-key)"?\s*[:=]\s*)("[^"]*"|'[^']*'|[^\s,}&]+)"#,
            "$1\"***\"",
        ),
        // Escaped JSON variant inside log strings: \"password\":\"…\"
        rule(
            r#"(?i)(\\"(?:password|secret|token|api_?key|access_token|refresh_token)\\"\s*:\s*\\")[^\\]*(\\")"#,
            "$1***$2",
        ),
    ]
});

/// Replace secrets in `input`.
pub fn scrub(input: &str) -> String {
    let mut out = input.to_string();
    for rule in RULES.iter() {
        if rule.re.is_match(&out) {
            out = rule.re.replace_all(&out, rule.replacement).into_owned();
        }
    }
    out
}

/// Truncate to at most `max` chars on a char boundary, marking the cut.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_common_secrets() {
        let s = scrub("postgresql://core_app_user:hunter2@postgres:5432/db");
        assert_eq!(s, "postgresql://core_app_user:***@postgres:5432/db");
        assert!(!scrub("Authorization: Bearer abcdefghijklmnop").contains("abcdefghijklmnop"));
        assert!(!scrub(r#"{"password":"p@ss","user":"x"}"#).contains("p@ss"));
        assert!(!scrub("token=abc123def&x=1").contains("abc123def"));
        assert!(!scrub("key gsk_FAKEtestKEY0000000000000").contains("FAKEtest"));
        assert!(!scrub(r#"body={\"access_token\":\"zzz\"}"#).contains("zzz"));
        assert_eq!(scrub("plain message"), "plain message");
    }

    #[test]
    fn truncate_marks_cut() {
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
