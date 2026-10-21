//! SQL `LIKE` pattern matching with a pre-compiled matcher.
//!
//! Common shapes (`abc`, `abc%`, `%abc`, `%abc%`) are recognised and turned
//! into byte-slice operations; anything else falls back to an iterative
//! wildcard matcher with single-star backtracking (linear in practice).

#[derive(Debug, Clone, PartialEq)]
pub enum LikeMatcher {
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    General(Vec<char>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Like {
    matcher: LikeMatcher,
    case_insensitive: bool,
}

impl Like {
    pub fn new(pattern: &str, case_insensitive: bool) -> Self {
        let pattern = if case_insensitive {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };
        let inner = pattern.trim_start_matches('%').trim_end_matches('%');
        let simple = !inner.contains(['%', '_']) && !pattern.contains('\\');
        let starts = pattern.starts_with('%');
        let ends = pattern.ends_with('%') && pattern.len() > 1;
        let matcher = if !simple {
            LikeMatcher::General(pattern.chars().collect())
        } else {
            match (starts, ends) {
                (false, false) => LikeMatcher::Exact(inner.to_string()),
                (false, true) => LikeMatcher::Prefix(inner.to_string()),
                (true, false) => LikeMatcher::Suffix(inner.to_string()),
                (true, true) => LikeMatcher::Contains(inner.to_string()),
            }
        };
        Like {
            matcher,
            case_insensitive,
        }
    }

    pub fn matches(&self, s: &str) -> bool {
        if self.case_insensitive {
            let lower = s.to_lowercase();
            self.matches_raw(&lower)
        } else {
            self.matches_raw(s)
        }
    }

    fn matches_raw(&self, s: &str) -> bool {
        match &self.matcher {
            LikeMatcher::Exact(p) => s == p,
            LikeMatcher::Prefix(p) => s.starts_with(p.as_str()),
            LikeMatcher::Suffix(p) => s.ends_with(p.as_str()),
            LikeMatcher::Contains(p) => s.contains(p.as_str()),
            LikeMatcher::General(p) => wildcard_match(p, s),
        }
    }
}

/// `%` matches any sequence, `_` any single character, `\` escapes.
fn wildcard_match(p: &[char], s: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && p[pi] == '%' {
            star = Some((pi, si));
            pi += 1;
            continue;
        }
        if pi < p.len() {
            let (lit, width) = if p[pi] == '\\' && pi + 1 < p.len() {
                (Some(p[pi + 1]), 2)
            } else if p[pi] == '_' {
                (None, 1)
            } else {
                (Some(p[pi]), 1)
            };
            if lit.is_none_or(|c| c == s[si]) {
                pi += width;
                si += 1;
                continue;
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == '%' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_shapes() {
        let cases = [
            ("abc", "abc", true),
            ("abc", "abcd", false),
            ("ab%", "abcd", true),
            ("%cd", "abcd", true),
            ("%bc%", "abcd", true),
            ("%", "", true),
            ("a_c", "abc", true),
            ("a_c", "ac", false),
            ("%a%b%c", "xxaxxbxxc", true),
            ("%a%b%c", "xxaxxbxxcx", false),
            ("100\\%", "100%", true),
            ("%special%requests%", "foo special bar requests baz", true),
        ];
        for (p, s, want) in cases {
            assert_eq!(Like::new(p, false).matches(s), want, "{s} LIKE {p}");
        }
        assert!(Like::new("AB%", true).matches("abc"));
    }
}
