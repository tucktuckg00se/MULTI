//! Word filter (FL-4): masks blocklisted and profane words in caption text,
//! in the source language and in every translation.
//!
//! Matching is whole-word, case-insensitive and accent-insensitive
//! (`PÉDÉ`, `pede` and `pédé` are the same word). A word is a run of letters
//! and digits, so attached punctuation never hides a word (`"fuck!"`,
//! `fuck's`). `*` in a list entry matches any run of letters (`ass*`).
//! Entries may be phrases (`blue waffle`). The allowlist wins over every list.
//!
//! Built-in lists (en, es, fr, de) are LDNOOBW's, see `data/README.md`.

use crate::config::{self, MaskStyle};
use std::collections::HashSet;

/// Built-in profanity list for a language, one entry per line.
pub fn builtin(lang: &str) -> Option<&'static str> {
    match lang {
        "en" => Some(include_str!("../data/profanity-en.txt")),
        "es" => Some(include_str!("../data/profanity-es.txt")),
        "fr" => Some(include_str!("../data/profanity-fr.txt")),
        "de" => Some(include_str!("../data/profanity-de.txt")),
        _ => None,
    }
}

/// Lowercase with accents removed (Latin scripts used by en/es/fr/de and
/// their neighbours).
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars().flat_map(char::to_lowercase) {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'ç' | 'ć' | 'č' | 'ĉ' | 'ċ' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ğ' | 'ĝ' | 'ġ' | 'ģ' => out.push('g'),
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => out.push('i'),
            'ł' | 'ľ' | 'ĺ' | 'ļ' => out.push('l'),
            'ñ' | 'ń' | 'ň' | 'ņ' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => out.push('o'),
            'ŕ' | 'ř' | 'ŗ' => out.push('r'),
            'ś' | 'š' | 'ş' | 'ŝ' | 'ș' => out.push('s'),
            'ť' | 'ţ' | 'ț' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ý' | 'ÿ' | 'ŷ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            _ => out.push(c),
        }
    }
    out
}

/// Words of `text`: (byte start, byte end, folded text).
fn tokens(text: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, i, fold(&text[s..i])));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, text.len(), fold(&text[s..])));
    }
    out
}

/// Why a list entry cannot be used, if it cannot.
pub fn entry_problem(entry: &str) -> Option<&'static str> {
    if entry
        .chars()
        .any(|c| !(c.is_alphanumeric() || c == '*' || c == ' ' || c == '\'' || c == '-'))
    {
        return Some("use letters, digits, spaces, ', - and * only");
    }
    let words: Vec<&str> = split_entry(entry).collect();
    if words.is_empty() {
        return Some("entry is empty");
    }
    if words.iter().any(|w| !w.chars().any(char::is_alphanumeric)) {
        return Some("every word needs at least one letter or digit besides *");
    }
    None
}

fn split_entry(entry: &str) -> impl Iterator<Item = &str> {
    entry
        .split(|c: char| !(c.is_alphanumeric() || c == '*'))
        .filter(|w| !w.is_empty())
}

/// `*` matches any run (possibly empty) of characters.
fn glob(pat: &[char], word: &[char]) -> bool {
    let (mut p, mut w) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while w < word.len() {
        if p < pat.len() && pat[p] != '*' && pat[p] == word[w] {
            p += 1;
            w += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some((p, w));
            p += 1;
        } else if let Some((sp, sw)) = star {
            p = sp + 1;
            w = sw + 1;
            star = Some((sp, sw + 1));
        } else {
            return false;
        }
    }
    pat[p..].iter().all(|&c| c == '*')
}

#[derive(Clone, Debug)]
enum Tok {
    Exact(String),
    Glob(Vec<char>),
}

impl Tok {
    fn matches(&self, word: &str) -> bool {
        match self {
            Tok::Exact(s) => s == word,
            Tok::Glob(p) => glob(p, &word.chars().collect::<Vec<_>>()),
        }
    }
}

/// A compiled word list.
#[derive(Clone, Debug, Default)]
struct List {
    /// One exact word: the common case, looked up directly.
    single: HashSet<String>,
    /// Globs and phrases.
    patterns: Vec<Vec<Tok>>,
}

impl List {
    fn add(&mut self, entry: &str) {
        if entry_problem(entry).is_some() {
            return;
        }
        let toks: Vec<Tok> = split_entry(entry)
            .map(|w| {
                let f = fold(w);
                if f.contains('*') {
                    Tok::Glob(f.chars().collect())
                } else {
                    Tok::Exact(f)
                }
            })
            .collect();
        match toks.as_slice() {
            [Tok::Exact(s)] => {
                self.single.insert(s.clone());
            }
            [] => {}
            _ => self.patterns.push(toks),
        }
    }

    /// Length (in words) of the longest entry matching at `words[0..]`.
    fn longest(&self, words: &[&str]) -> Option<usize> {
        let mut best = words
            .first()
            .is_some_and(|w| self.single.contains(*w))
            .then_some(1);
        for p in &self.patterns {
            if p.len() <= words.len()
                && best.is_none_or(|b| p.len() > b)
                && p.iter().zip(words).all(|(t, w)| t.matches(w))
            {
                best = Some(p.len());
            }
        }
        best
    }

    /// Whether an entry matches exactly `words`.
    fn exact(&self, words: &[&str]) -> bool {
        (words.len() == 1 && self.single.contains(words[0]))
            || self
                .patterns
                .iter()
                .any(|p| p.len() == words.len() && p.iter().zip(words).all(|(t, w)| t.matches(w)))
    }
}

/// The filter for one caption lane.
#[derive(Clone, Debug)]
pub struct WordFilter {
    style: MaskStyle,
    block: List,
    allow: List,
}

impl WordFilter {
    /// Built-in lists for `langs` (when `cfg.profanity`), plus the user's
    /// blocklist, minus the allowlist.
    pub fn new(cfg: &config::Filter, langs: &[&str]) -> Self {
        let mut block = List::default();
        if cfg.profanity {
            for l in langs {
                for line in builtin(l).unwrap_or("").lines() {
                    block.add(line.trim());
                }
            }
        }
        for e in &cfg.blocklist {
            block.add(e.trim());
        }
        let mut allow = List::default();
        for e in &cfg.allowlist {
            allow.add(e.trim());
        }
        Self {
            style: cfg.mask_style,
            block,
            allow,
        }
    }

    /// Byte ranges of blocked words and phrases in `text`.
    pub fn find(&self, text: &str) -> Vec<(usize, usize)> {
        let toks = tokens(text);
        let words: Vec<&str> = toks.iter().map(|t| t.2.as_str()).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < words.len() {
            match self.block.longest(&words[i..]) {
                Some(n) if !self.allow.exact(&words[i..i + n]) => {
                    out.push((toks[i].0, toks[i + n - 1].1));
                    i += n;
                }
                _ => i += 1,
            }
        }
        out
    }

    /// `text` with every blocked word masked.
    pub fn apply(&self, text: &str) -> String {
        let spans = self.find(text);
        if spans.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (s, e) in spans {
            out.push_str(&text[at..s]);
            let word = &text[s..e];
            match self.style {
                MaskStyle::Asterisks => out.extend(
                    word.chars()
                        .map(|c| if c.is_alphanumeric() { '*' } else { c }),
                ),
                MaskStyle::FirstLetter => {
                    let mut first = true;
                    for c in word.chars() {
                        if c.is_alphanumeric() {
                            out.push(if first { c } else { '*' });
                            first = false;
                        } else {
                            out.push(c);
                        }
                    }
                }
                MaskStyle::Bleep => out.push_str("[bleep]"),
                MaskStyle::Drop => {}
            }
            at = e;
        }
        out.push_str(&text[at..]);
        if self.style == MaskStyle::Drop {
            tidy(&out)
        } else {
            out
        }
    }
}

/// Collapses the spaces a dropped word leaves behind.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == ' ' && (out.is_empty() || out.ends_with(' ')) {
            continue;
        }
        if matches!(c, ',' | '.' | '!' | '?' | ';' | ':') && out.ends_with(' ') {
            out.pop();
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::config::Filter;

    fn filter(style: MaskStyle, block: &[&str], allow: &[&str]) -> WordFilter {
        let cfg = Filter {
            profanity: true,
            mask_style: style,
            blocklist: block.iter().map(|s| s.to_string()).collect(),
            allowlist: allow.iter().map(|s| s.to_string()).collect(),
        };
        WordFilter::new(&cfg, &["en", "es", "fr", "de"])
    }

    #[test]
    fn mask_styles() {
        let s = "well, Fuck! that's it";
        let m = |st| filter(st, &[], &[]).apply(s);
        assert_eq!(m(MaskStyle::Asterisks), "well, ****! that's it");
        assert_eq!(m(MaskStyle::FirstLetter), "well, F***! that's it");
        assert_eq!(m(MaskStyle::Bleep), "well, [bleep]! that's it");
        assert_eq!(m(MaskStyle::Drop), "well,! that's it");
    }

    #[test]
    fn whole_words_only() {
        let f = filter(MaskStyle::Asterisks, &["acme"], &[]);
        assert_eq!(f.apply("class assessment ACMEs"), "class assessment ACMEs");
        assert_eq!(f.apply("Acme's (acme)"), "****'s (****)");
    }

    #[test]
    fn wildcards_phrases_and_allowlist() {
        let f = filter(
            MaskStyle::Bleep,
            &["zorp*", "*blat", "big bad*"],
            &["zorpina"],
        );
        assert_eq!(f.apply("zorp zorping zorpina"), "[bleep] [bleep] zorpina");
        assert_eq!(f.apply("kablat blat blatter"), "[bleep] [bleep] blatter");
        assert_eq!(f.apply("a Big  Badger big"), "a [bleep] big");
        let none = WordFilter::new(
            &Filter {
                profanity: false,
                ..Filter::default()
            },
            &["en"],
        );
        assert_eq!(none.apply("fuck"), "fuck");
    }

    #[test]
    fn accents_fold_both_ways() {
        let f = filter(MaskStyle::Asterisks, &["fenêtre", "strasse"], &[]);
        assert_eq!(
            f.apply("FENETRE Fenêtre fenétre"),
            "******* ******* *******"
        );
        assert_eq!(f.apply("Straße"), "******");
        assert_eq!(fold("PÉDÉ Möse ŒIL"), "pede mose oeil");
    }

    #[test]
    fn bad_entries_are_ignored() {
        assert!(entry_problem("*").is_some());
        assert!(entry_problem("s&m").is_some());
        assert!(entry_problem("  ").is_some());
        assert!(entry_problem("don't").is_none());
        let f = filter(MaskStyle::Asterisks, &["*", "a$$"], &[]);
        assert_eq!(f.apply("a b c"), "a b c");
    }

    /// Case variants, accents added or removed, attached punctuation.
    fn variants(word: &str) -> Vec<String> {
        let upper = word.to_uppercase();
        let mut title = String::new();
        for (i, c) in word.chars().enumerate() {
            if i == 0 {
                title.extend(c.to_uppercase());
            } else {
                title.push(c);
            }
        }
        let folded = fold(word);
        let accented: String = folded
            .chars()
            .map(|c| match c {
                'a' => 'á',
                'e' => 'è',
                'o' => 'ö',
                'u' => 'ü',
                _ => c,
            })
            .collect();
        let mut out = Vec::new();
        for w in [word.to_string(), upper, title, folded, accented] {
            for (pre, post) in [
                ("", ""),
                ("\"", "!"),
                ("(", ")."),
                ("¿", "?"),
                ("", "'s"),
                ("-", ","),
            ] {
                out.push(format!("so {pre}{w}{post} then"));
            }
        }
        out
    }

    /// FL-4: a listed word never survives, in any language, casing, accent
    /// form, or with punctuation attached, in any mask style.
    #[test]
    fn no_listed_word_survives() {
        let styles = [
            MaskStyle::Asterisks,
            MaskStyle::FirstLetter,
            MaskStyle::Bleep,
            MaskStyle::Drop,
        ];
        let mut checked = 0;
        for lang in ["en", "es", "fr", "de"] {
            for entry in builtin(lang).unwrap().lines().map(str::trim) {
                if entry_problem(entry).is_some() {
                    continue;
                }
                let target = fold(entry);
                let target_words: Vec<String> = split_entry(&target).map(String::from).collect();
                for st in styles {
                    let f = filter(st, &[], &[]);
                    for text in variants(entry) {
                        let out = f.apply(&text);
                        let words: Vec<String> = tokens(&out).into_iter().map(|t| t.2).collect();
                        let survives = words
                            .windows(target_words.len())
                            .any(|w| w == target_words.as_slice());
                        assert!(
                            !survives,
                            "[{lang}] {entry:?} survived: {text:?} -> {out:?}"
                        );
                        assert!(f.find(&out).is_empty(), "{out:?} still matches");
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 10_000, "{checked}");
    }
}
