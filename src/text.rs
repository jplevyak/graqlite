//! Full-text search: the words of values, and the values containing words.
//!
//! A value's *terms* are its runs of alphanumeric characters, lower cased,
//! with no stemming or stop words.  Values are interned and never change, so
//! the index is append only: [`BuiltinIndex::Text`] holds, under the uuid of
//! each term, the ids of the values containing it, added as each value is
//! appended.  It names values whether or not a link still does: a search
//! joins a pattern that decides that.
//!
//! - **What is indexed:** every value that is UTF-8 text, up to
//!   [`MAX_INDEXED`] bytes.  Vectors and other binary values have no terms.
//!   A node's key is a value linked to it by `graqlite:key`, so a search of
//!   any predicate finds that too.
//! - **A [`Query`]** is alternatives separated by `|`, each of terms all of
//!   which must be in a value: `paris louvre | orsay`.
//! - **[`Matches`]** reads the values of a query newest first, as needed:
//!   each alternative walks its rarest term's list and looks for each id in
//!   the others, so a term with a million postings costs what the rarest
//!   term does.
//!
//! [`BuiltinIndex::Text`]: crate::tuple_index::BuiltinIndex::Text

use crate::local_table::{Ids, TableRef};
use crate::record::Uuid;

/// The bytes of a value that are indexed: terms past them aren't found.
pub const MAX_INDEXED: usize = 64 * 1024;

/// How many ids of a list are read looking for one before seeking to it.
const SKIP_READS: usize = 8;

/// The key of `term` in the index: every term's is a hash, so a long one
/// takes no more room.
pub fn key(term: &str) -> Uuid {
    Uuid::of(term.as_bytes())
}

/// The distinct terms of `text`, sorted.
fn terms_of(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// The part of `text` that is indexed: its first [`MAX_INDEXED`] bytes, less
/// a word they cut in two.
fn indexed(text: &str) -> &str {
    if text.len() <= MAX_INDEXED {
        return text;
    }
    let mut end = MAX_INDEXED;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[end..]
        .chars()
        .next()
        .is_some_and(char::is_alphanumeric);
    if cut {
        text[..end].trim_end_matches(char::is_alphanumeric)
    } else {
        &text[..end]
    }
}

/// The distinct terms of a value's `data`, sorted: none if it isn't text.
pub fn terms(data: &[u8]) -> Vec<String> {
    match std::str::from_utf8(data) {
        Ok(text) => terms_of(indexed(text)),
        Err(_) => Vec::new(),
    }
}

/// What a text search looks for; see the [module](self) docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// The alternatives, each of distinct terms.  None have no terms.
    any: Vec<Vec<String>>,
}

impl Query {
    /// The query `text` writes.  With no words in it, it matches nothing.
    pub fn parse(text: &str) -> Query {
        Query {
            any: text
                .split('|')
                .map(terms_of)
                .filter(|terms| !terms.is_empty())
                .collect(),
        }
    }

    /// The terms of each alternative.
    pub fn alternatives(&self) -> &[Vec<String>] {
        &self.any
    }

    /// Whether a value holding `data` matches, as the index would find it.
    pub fn matches(&self, data: &[u8]) -> bool {
        if self.any.is_empty() {
            return false;
        }
        let Ok(text) = std::str::from_utf8(data) else {
            return false;
        };
        // Words found so far, for each alternative's terms, without keeping
        // the value's: this runs for every row a pattern reaches.
        let mut found: Vec<Vec<bool>> = self.any.iter().map(|all| vec![false; all.len()]).collect();
        let mut lower = String::new();
        for word in indexed(text)
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            let word = if word.is_ascii() {
                lower.clear();
                lower.push_str(word);
                lower.make_ascii_lowercase();
                lower.as_str()
            } else {
                lower = word.to_lowercase();
                lower.as_str()
            };
            for (all, found) in self.any.iter().zip(&mut found) {
                if let Ok(i) = all.binary_search_by(|t| t.as_str().cmp(word)) {
                    found[i] = true;
                    if found.iter().all(|&f| f) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The number of values each term of each alternative is in, and an
    /// estimate of the values matching: each alternative's rarest term's.
    pub fn counts(&self, table: &TableRef<'_>) -> (Vec<(String, u64)>, u64) {
        let mut each = Vec::new();
        let mut total = 0;
        for all in &self.any {
            let counts: Vec<u64> = all.iter().map(|t| table.count(key(t))).collect();
            each.extend(all.iter().cloned().zip(counts.iter().copied()));
            total += counts.into_iter().min().unwrap_or(0);
        }
        (each, total)
    }

    /// The values matching with ids at most `last`, newest first.
    pub fn search(&self, table: &TableRef<'_>, last: u64) -> Matches {
        let mut alternatives: Vec<All> = self
            .any
            .iter()
            .filter_map(|all| All::new(table, all, last))
            .collect();
        let heads = alternatives.iter_mut().map(|a| a.next(table)).collect();
        Matches {
            alternatives,
            heads,
        }
    }
}

/// A term's list, being walked down from the ids searched for.
struct Walk {
    key: Uuid,
    ids: Ids,
    head: Option<u64>,
}

impl Walk {
    /// Whether the list has `id`, which is below any id asked about before.
    fn has(&mut self, table: &TableRef<'_>, id: u64) -> bool {
        for _ in 0..SKIP_READS {
            match self.head {
                Some(h) if h > id => self.head = self.ids.next(),
                head => return head == Some(id),
            }
        }
        if self.head.is_some_and(|h| h > id) {
            self.ids = table.seek_rev(self.key, id);
            self.head = self.ids.next();
        }
        self.head == Some(id)
    }
}

/// The ids in every list of one alternative, newest first.
struct All {
    rarest: Ids,
    others: Vec<Walk>,
}

impl All {
    /// `None` if a term has no values, so that nothing can match.
    fn new(table: &TableRef<'_>, terms: &[String], last: u64) -> Option<All> {
        let mut keys: Vec<(u64, Uuid)> = terms
            .iter()
            .map(|t| (table.count(key(t)), key(t)))
            .collect();
        keys.sort_unstable_by_key(|k| k.0);
        if keys.first().is_none_or(|k| k.0 == 0) {
            return None;
        }
        let rarest = table.seek_rev(keys[0].1, last);
        let others = keys[1..]
            .iter()
            .map(|&(_, key)| {
                let mut ids = table.seek_rev(key, last);
                let head = ids.next();
                Walk { key, ids, head }
            })
            .collect();
        Some(All { rarest, others })
    }

    fn next(&mut self, table: &TableRef<'_>) -> Option<u64> {
        let mut id = self.rarest.next();
        while let Some(i) = id {
            if self.others.iter_mut().all(|w| w.has(table, i)) {
                return Some(i);
            }
            id = self.rarest.next();
        }
        None
    }
}

/// The ids of the values matching a query, newest first; see
/// [`Query::search`].  An id may name no value, after a rollback or a crash
/// (see [`local`](crate::local)): callers check.
pub struct Matches {
    alternatives: Vec<All>,
    /// Each alternative's next id.
    heads: Vec<Option<u64>>,
}

impl Matches {
    /// The next id, reading `table`, the one searched.
    pub fn next(&mut self, table: &TableRef<'_>) -> Option<u64> {
        let id = self.heads.iter().copied().flatten().max()?;
        for (head, alternative) in self.heads.iter_mut().zip(&mut self.alternatives) {
            if *head == Some(id) {
                *head = alternative.next(table);
            }
        }
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        terms(text.as_bytes())
    }

    #[test]
    fn terms_are_lower_cased_words() {
        assert_eq!(words("Hello, hello WORLD!"), ["hello", "world"]);
        assert_eq!(words("don't stop-now"), ["don", "now", "stop", "t"]);
        assert_eq!(words("in 2019, a1b2"), ["2019", "a1b2", "in"]);
        assert!(words(" ... !").is_empty());
        assert!(words("").is_empty());
    }

    #[test]
    fn terms_of_unicode_text() {
        assert_eq!(words("Ünïcode ÉCOLE"), ["école", "ünïcode"]);
        assert_eq!(words("ΣΑΣ σας"), ["σας"]);
        assert_eq!(words("Москва, МОСКВА"), ["москва"]);
        // Runs of letters with no spaces are one word, whatever the script.
        assert_eq!(words("東京 タワー"), ["タワー", "東京"]);
        assert_eq!(words("a—b…c"), ["a", "b", "c"]);
    }

    #[test]
    fn data_that_is_not_text_has_no_terms() {
        assert!(terms(&[0xff, b'a', b'b']).is_empty());
        assert!(terms(&crate::vector::encode(&[1.0, 2.0])).is_empty());
    }

    #[test]
    fn long_values_are_indexed_to_the_limit() {
        // The word across the limit isn't taken for a shorter one.
        let mut text = "a ".repeat(MAX_INDEXED / 2 - 2);
        text.push_str("near words");
        text.push_str(" beyond");
        assert_eq!(text.len(), MAX_INDEXED + 13);
        let found = terms(text.as_bytes());
        assert!(found.contains(&"a".to_string()));
        assert!(found.contains(&"near".to_string()));
        assert!(
            !found
                .iter()
                .any(|t| "words".starts_with(t.as_str()) && t != "w" || t == "beyond")
        );
        // A limit inside a character is moved back.
        let text = "é".repeat(MAX_INDEXED);
        assert!(terms(text.as_bytes()).is_empty());
        let text = format!("{} {}", "é".repeat(MAX_INDEXED / 2 - 1), "é".repeat(9));
        assert_eq!(terms(text.as_bytes()), ["é".repeat(MAX_INDEXED / 2 - 1)]);
    }

    #[test]
    fn queries_match_all_terms_of_any_alternative() {
        let q = Query::parse("Paris louvre | Orsay");
        assert_eq!(q.alternatives().len(), 2);
        assert!(q.matches(b"the Louvre, in PARIS"));
        assert!(q.matches(b"musee d'orsay"));
        assert!(!q.matches(b"paris alone"));
        assert!(!q.matches(b"louvres of paris"));
        for empty in ["", "  ", "|", "... | !"] {
            let q = Query::parse(empty);
            assert!(q.alternatives().is_empty());
            assert!(!q.matches(b"anything"));
        }
        assert_eq!(Query::parse("a | ").alternatives().len(), 1);
    }
}
