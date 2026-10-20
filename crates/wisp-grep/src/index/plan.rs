//! Turns rg patterns into a trigram query that every matching file satisfies.
//!
//! The query is a necessary condition only: a file whose folded content lacks
//! the query's trigrams cannot match, and anything the planner cannot reason
//! about becomes `All`. This follows the literal-set approach of Russ Cox's
//! codesearch: track the exact set of strings a subexpression can match while
//! it stays small, and turn it into trigram conjunctions when it does not.

use regex_syntax::ParserBuilder;
use regex_syntax::hir::{Class, Hir, HirKind};

use super::fold;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Query {
    /// Every file may match.
    All,
    /// No file can match.
    None,
    Tri(u32),
    /// A trigram followed by this byte somewhere in the file (checked with
    /// the posting's next-byte mask, so it may still be a false positive).
    TriNext(u32, u8),
    And(Vec<Query>),
    Or(Vec<Query>),
}

pub(crate) struct PlanOptions {
    pub(crate) fixed_strings: bool,
    /// True for `-i` and smart case: over-approximating smart case is safe.
    pub(crate) case_insensitive: bool,
    pub(crate) unicode: bool,
}

const MAX_SET: usize = 64;
const MAX_CLASS: u32 = 16;

pub(crate) fn plan(patterns: &[String], opts: &PlanOptions) -> Query {
    let mut alternatives = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let source = if opts.fixed_strings { regex_syntax::escape(pattern) } else { pattern.clone() };
        let parsed = ParserBuilder::new()
            .utf8(false)
            .unicode(opts.unicode)
            .case_insensitive(opts.case_insensitive)
            .multi_line(true)
            .octal(false)
            .build()
            .parse(&source);
        match parsed {
            Ok(hir) => alternatives.push(to_query(analyze(&hir))),
            // rg may still accept the pattern (for example through PCRE2
            // fallback); without an analysis nothing can be pruned.
            Err(_) => return Query::All,
        }
    }
    or(alternatives)
}

/// Compact text form for the daemon protocol: `*` all, `0` none, `t` plus six
/// hex digits for a trigram, `n` plus eight for a trigram and its next byte,
/// `&(...)` and `|(...)` for conjunctions.
pub(crate) fn encode(query: &Query) -> String {
    let mut out = String::new();
    fn go(q: &Query, out: &mut String) {
        match q {
            Query::All => out.push('*'),
            Query::None => out.push('0'),
            Query::Tri(k) => out.push_str(&format!("t{k:06x}")),
            Query::TriNext(k, b) => out.push_str(&format!("n{k:06x}{b:02x}")),
            Query::And(parts) | Query::Or(parts) => {
                out.push_str(if matches!(q, Query::And(_)) { "&(" } else { "|(" });
                parts.iter().for_each(|p| go(p, out));
                out.push(')');
            }
        }
    }
    go(query, &mut out);
    out
}

pub(crate) fn decode(text: &str) -> Option<Query> {
    fn go(s: &[u8], pos: &mut usize) -> Option<Query> {
        let c = *s.get(*pos)?;
        *pos += 1;
        match c {
            b'*' => Some(Query::All),
            b'0' => Some(Query::None),
            b't' => {
                let hex = std::str::from_utf8(s.get(*pos..*pos + 6)?).ok()?;
                *pos += 6;
                Some(Query::Tri(u32::from_str_radix(hex, 16).ok()?))
            }
            b'n' => {
                let hex = std::str::from_utf8(s.get(*pos..*pos + 8)?).ok()?;
                *pos += 8;
                let key = u32::from_str_radix(&hex[..6], 16).ok()?;
                let next = u8::from_str_radix(&hex[6..], 16).ok()?;
                Some(Query::TriNext(key, next))
            }
            b'&' | b'|' => {
                if s.get(*pos) != Some(&b'(') {
                    return None;
                }
                *pos += 1;
                let mut parts = Vec::new();
                while s.get(*pos) != Some(&b')') {
                    parts.push(go(s, pos)?);
                }
                *pos += 1;
                Some(if c == b'&' { Query::And(parts) } else { Query::Or(parts) })
            }
            _ => None,
        }
    }
    let mut pos = 0;
    let query = go(text.as_bytes(), &mut pos)?;
    (pos == text.len()).then_some(query)
}

struct Info {
    /// Folded strings, one of which the subexpression matches exactly.
    exact: Option<Vec<Vec<u8>>>,
    /// Additional necessary condition.
    query: Query,
}

impl Info {
    fn exact(set: Vec<Vec<u8>>) -> Info {
        Info { exact: Some(set), query: Query::All }
    }
    fn unknown() -> Info {
        Info { exact: None, query: Query::All }
    }
}

fn analyze(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::exact(vec![Vec::new()]),
        HirKind::Literal(lit) => {
            if fold::fold_is_context_free(&lit.0) {
                Info::exact(vec![fold::fold(&lit.0)])
            } else {
                Info::unknown()
            }
        }
        HirKind::Class(Class::Unicode(class)) => {
            let count: u32 = class.ranges().iter().map(|r| r.end() as u32 - r.start() as u32 + 1).sum();
            if count > MAX_CLASS {
                return Info::unknown();
            }
            let mut set = Vec::new();
            for range in class.ranges() {
                for cp in range.start() as u32..=range.end() as u32 {
                    if let Some(ch) = char::from_u32(cp) {
                        let mut buf = [0; 4];
                        set.push(fold::fold(ch.encode_utf8(&mut buf).as_bytes()));
                    }
                }
            }
            Info::exact(dedup(set))
        }
        HirKind::Class(Class::Bytes(class)) => {
            let count: u32 = class.ranges().iter().map(|r| u32::from(r.end()) - u32::from(r.start()) + 1).sum();
            if count > MAX_CLASS {
                return Info::unknown();
            }
            let mut set = Vec::new();
            for range in class.ranges() {
                for b in range.start()..=range.end() {
                    if !fold::fold_is_context_free(&[b]) {
                        return Info::unknown();
                    }
                    set.push(fold::fold(&[b]));
                }
            }
            Info::exact(dedup(set))
        }
        HirKind::Capture(cap) => analyze(&cap.sub),
        HirKind::Repetition(rep) => {
            if rep.min == 0 {
                Info::unknown()
            } else if rep.min == 1 && rep.max == Some(1) {
                analyze(&rep.sub)
            } else {
                // At least one copy must occur somewhere.
                Info { exact: None, query: to_query(analyze(&rep.sub)) }
            }
        }
        HirKind::Concat(subs) => concat(subs),
        HirKind::Alternation(subs) => {
            let infos: Vec<Info> = subs.iter().map(analyze).collect();
            if infos.iter().all(|i| i.exact.is_some()) {
                let union = dedup(infos.into_iter().flat_map(|i| i.exact.unwrap()).collect());
                if union.len() <= MAX_SET {
                    return Info::exact(union);
                }
                return Info { exact: None, query: exact_query(&union) };
            }
            Info { exact: None, query: or(infos.into_iter().map(to_query).collect()) }
        }
    }
}

fn concat(subs: &[Hir]) -> Info {
    let mut current: Vec<Vec<u8>> = vec![Vec::new()];
    let mut conditions = Vec::new();
    let mut flushed = false;
    for sub in subs {
        let info = analyze(sub);
        conditions.push(info.query);
        match info.exact {
            Some(set) => {
                let cross = product(&current, &set);
                if cross.len() <= MAX_SET {
                    current = cross;
                    continue;
                }
                conditions.push(exact_query(&current));
                flushed = true;
                // Keep the last two bytes of each string so trigrams that
                // span the boundary still count: the text contains some
                // `s + t`, hence also `tail(s) + t`.
                let tails = dedup(current.iter().map(|s| s[s.len().saturating_sub(2)..].to_vec()).collect());
                let bridged = product(&tails, &set);
                current = if bridged.len() <= MAX_SET { bridged } else { set };
            }
            None => {
                conditions.push(exact_query(&current));
                flushed = true;
                current = vec![Vec::new()];
            }
        }
    }
    if !flushed && conditions.iter().all(|q| *q == Query::All) {
        return Info::exact(current);
    }
    conditions.push(exact_query(&current));
    Info { exact: None, query: and(conditions) }
}

fn to_query(info: Info) -> Query {
    match info.exact {
        Some(set) => and(vec![info.query, exact_query(&set)]),
        None => info.query,
    }
}

fn exact_query(set: &[Vec<u8>]) -> Query {
    if set.iter().any(|s| s.len() < 3) {
        return Query::All;
    }
    or(set
        .iter()
        .map(|s| {
            // Each trigram except the last must be followed by the next byte
            // of the string.
            and(s
                .windows(3)
                .enumerate()
                .map(|(i, w)| {
                    let key = fold::key(w[0], w[1], w[2]);
                    match s.get(i + 3) {
                        Some(&next) => Query::TriNext(key, next),
                        None => Query::Tri(key),
                    }
                })
                .collect())
        })
        .collect())
}

fn product(left: &[Vec<u8>], right: &[Vec<u8>]) -> Vec<Vec<u8>> {
    if left.len().saturating_mul(right.len()) > MAX_SET {
        // Callers only need to know the product is too large.
        return vec![Vec::new(); MAX_SET + 1];
    }
    let mut out = Vec::with_capacity(left.len() * right.len());
    for l in left {
        for r in right {
            let mut s = l.clone();
            s.extend_from_slice(r);
            out.push(s);
        }
    }
    dedup(out)
}

fn dedup(mut set: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    set.sort_unstable();
    set.dedup();
    set
}

fn and(parts: Vec<Query>) -> Query {
    let mut out = Vec::new();
    for part in parts {
        match part {
            Query::All => {}
            Query::None => return Query::None,
            Query::And(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    out.sort_by_key(|q| matches!(q, Query::Or(_)));
    let mut seen = Vec::new();
    out.retain(|q| {
        if seen.contains(q) {
            false
        } else {
            seen.push(q.clone());
            true
        }
    });
    match out.len() {
        0 => Query::All,
        1 => out.pop().unwrap(),
        _ => Query::And(out),
    }
}

fn or(parts: Vec<Query>) -> Query {
    let mut out = Vec::new();
    for part in parts {
        match part {
            Query::All => return Query::All,
            Query::None => {}
            Query::Or(inner) => out.extend(inner),
            other => out.push(other),
        }
    }
    out.dedup();
    match out.len() {
        0 => Query::None,
        1 => out.pop().unwrap(),
        _ => Query::Or(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pattern: &str, ci: bool) -> Query {
        plan(&[pattern.to_string()], &PlanOptions { fixed_strings: false, case_insensitive: ci, unicode: true })
    }

    fn tri(s: &str) -> Query {
        let b = s.as_bytes();
        Query::Tri(fold::key(b[0], b[1], b[2]))
    }

    /// The trigram at the start of `s`, followed by `s[3]`.
    fn next(s: &str) -> Query {
        let b = s.as_bytes();
        Query::TriNext(fold::key(b[0], b[1], b[2]), b[3])
    }

    #[test]
    fn literal_needs_all_its_trigrams() {
        assert_eq!(p("abcd", false), Query::And(vec![next("abcd"), tri("bcd")]));
    }

    #[test]
    fn case_insensitive_folds_ascii_and_kelvin() {
        assert_eq!(p("KAB", true), tri("kab"));
    }

    #[test]
    fn non_ascii_case_variants_are_enumerated() {
        assert_eq!(p("éab", true), Query::Or(vec![
            Query::And(vec![Query::TriNext(fold::key(0xC3, 0x89, b'a'), b'b'), Query::Tri(fold::key(0x89, b'a', b'b'))]),
            Query::And(vec![Query::TriNext(fold::key(0xC3, 0xA9, b'a'), b'b'), Query::Tri(fold::key(0xA9, b'a', b'b'))]),
        ]));
    }

    #[test]
    fn short_and_unbounded_patterns_are_all() {
        assert_eq!(p("ab", false), Query::All);
        assert_eq!(p(r"\w+", false), Query::All);
        assert_eq!(p("a.*b", false), Query::All);
    }

    #[test]
    fn alternation_ors_branches() {
        assert_eq!(p("abc|xyz", false), Query::Or(vec![tri("abc"), tri("xyz")]));
        assert_eq!(p("abc|x", false), Query::All);
    }

    #[test]
    fn concatenation_keeps_boundary_trigrams() {
        assert_eq!(p("ab[cd]ef", false), Query::Or(vec![
            Query::And(vec![next("abce"), next("bcef"), tri("cef")]),
            Query::And(vec![next("abde"), next("bdef"), tri("def")]),
        ]));
    }

    #[test]
    fn required_repetition_keeps_its_literal() {
        assert_eq!(p("(?:abc)+x", false), tri("abc"));
    }

    #[test]
    fn impossible_class_is_none() {
        assert_eq!(p(r"[^\x00-\x{10FFFF}]abc", false), Query::None);
    }

    #[test]
    fn encoding_round_trips() {
        let q = p("ab[cd]ef|xyz", true);
        assert_eq!(decode(&encode(&q)), Some(q));
        assert_eq!(decode("&(t616263"), None);
    }

    #[test]
    fn unparseable_is_all() {
        assert_eq!(p("(?<=a)bcd", false), Query::All);
    }
}
