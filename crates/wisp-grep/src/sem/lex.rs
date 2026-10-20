//! Lexical helpers: word splitting, stop words, exact-identifier priority.
//!
//! Hand-rolled (no `regex` dependency): camel-case splitting, Unicode-aware
//! alphanumeric tokenization, and substring identifier matching with word
//! boundary checks. Mirrors the reference behavior (camel-split, lowercase,
//! stop-filtered words counted over `path + content`) without depending on
//! any external ranker.

/// English stop words. Kept identical for queries and chunks.
const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "do", "does", "for",
    "from", "had", "has", "have", "how", "i", "if", "in", "into", "is", "it", "its", "me", "not",
    "of", "on", "or", "our", "should", "so", "that", "the", "their", "them", "there", "these",
    "they", "this", "to", "use", "using", "was", "we", "what", "when", "where", "which", "who",
    "will", "with", "would", "you", "your", "function", "description", "purpose", "input",
    "output",
];

fn is_stop(w: &str) -> bool {
    STOP.contains(&w)
}

/// Split `text` into words: camel-case boundaries (including acronym
/// boundaries like `HTTPRequest` -> `HTTP Request`), lowercased, split on
/// anything that is not a Unicode letter or number. Single characters and
/// stop words are dropped.
pub(crate) fn words(text: &str) -> Vec<String> {
    let mut split = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for i in 0..chars.len() {
        let c = chars[i];
        if i > 0 && c.is_uppercase() {
            let prev = chars[i - 1];
            let next = chars.get(i + 1);
            // lowerACRONYM boundary (`queryVector` -> `query Vector`) or
            // acronymWord boundary (`HTTPRequest` -> `HTTP Request`).
            let boundary = prev.is_lowercase()
                || prev.is_numeric()
                || (prev.is_uppercase()
                    && matches!(next, Some(n) if n.is_lowercase()));
            if boundary {
                split.push(' ');
            }
        }
        split.push(c);
    }
    split
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() > 1 && !is_stop(w))
        .map(str::to_owned)
        .collect()
}

/// Identifier-ish literals in `query` that deserve exact-match priority:
/// the whole query itself, or tokens containing `_`/`$`, camel case, or
/// digits (the names agents actually search for).
pub(crate) fn identifier_literals(query: &str) -> Vec<String> {
    let q = query.trim();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut flush = |cur: &mut String| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    for c in q.chars() {
        if c.is_alphanumeric() || c == '_' || c == '$' {
            cur.push(c);
        } else {
            flush(&mut cur);
        }
    }
    flush(&mut cur);
    out.into_iter()
        .filter(|w| {
            *w == q
                || w.contains(['_', '$'])
                || w.chars().any(|c| c.is_numeric())
                || {
                    let cs: Vec<char> = w.chars().collect();
                    cs.windows(2).any(|p| {
                        (p[0].is_lowercase() || p[0].is_numeric()) && p[1].is_uppercase()
                    })
                }
        })
        .collect()
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// True when `literal` occurs in `content` with non-word characters (or
/// text edges) on both sides. Case-sensitive: exact identifiers only.
pub(crate) fn contains_identifier(content: &str, literal: &str) -> bool {
    if literal.is_empty() || literal.len() > content.len() {
        return false;
    }
    let lit: Vec<char> = literal.chars().collect();
    let txt: Vec<char> = content.chars().collect();
    if lit.is_empty() || txt.len() < lit.len() {
        return false;
    }
    for i in 0..=txt.len() - lit.len() {
        if txt[i..i + lit.len()] == lit[..] {
            let left_ok = i == 0 || !is_word_char(txt[i - 1]);
            let right_ok = i + lit.len() == txt.len() || !is_word_char(txt[i + lit.len()]);
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_splits() {
        assert!(words("queryVector HTTPRequest done").contains(&"query".to_owned()));
        assert!(words("queryVector HTTPRequest done").contains(&"vector".to_owned()));
        assert!(words("queryVector HTTPRequest done").contains(&"http".to_owned()));
        assert!(words("queryVector HTTPRequest done").contains(&"request".to_owned()));
    }

    #[test]
    fn identifier_boundaries() {
        assert!(contains_identifier("let shouldBill = 1;", "shouldBill"));
        assert!(!contains_identifier("let shouldBills = 1;", "shouldBill"));
        assert!(!contains_identifier("xshouldBill", "shouldBill"));
    }
}
