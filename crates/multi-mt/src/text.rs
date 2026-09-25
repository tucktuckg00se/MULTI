//! Text handling around the translator, ported from the S5 spike.

/// Longest clause accepted, in bytes; longer input is refused, not truncated.
pub const MAX_INPUT_BYTES: usize = 4096;

/// Decode length cap: about three subwords per source word plus slack, so a
/// model that starts repeating itself stops quickly.
pub fn max_decoding_length(src_words: usize) -> usize {
    src_words.saturating_mul(4).saturating_add(16).min(256)
}

/// Splits after `. ! ?` when followed by whitespace and an upper-case or
/// opening-punctuation character; skips common abbreviations. opus-mt
/// translates single sentences much better than run-ons.
pub fn sentences(text: &str) -> Vec<&str> {
    const ABBR: [&str; 8] = ["Mr.", "Mrs.", "Ms.", "Dr.", "St.", "Jr.", "a.m.", "p.m."];
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for w in 0..chars.len() {
        let Some(&(i, c)) = chars.get(w) else { break };
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let Some(&(j, ws)) = chars.get(w + 1) else {
            continue;
        };
        let Some(&(_, next)) = chars.get(w + 2) else {
            continue;
        };
        if !ws.is_whitespace() || !(next.is_uppercase() || matches!(next, '¿' | '¡' | '"' | '“'))
        {
            continue;
        }
        let Some(head) = text.get(start..=i) else {
            continue;
        };
        if ABBR.iter().any(|a| head.ends_with(a)) {
            continue;
        }
        let head = head.trim();
        if !head.is_empty() {
            out.push(head);
        }
        start = j;
    }
    let tail = text.get(start..).unwrap_or_default().trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_sentences_not_abbreviations() {
        assert_eq!(
            sentences("Cold, is it, my darling? Bless your sweet face."),
            ["Cold, is it, my darling?", "Bless your sweet face."]
        );
        assert_eq!(
            sentences("Thank you, Mr. Chen. We'll ask staff."),
            ["Thank you, Mr. Chen.", "We'll ask staff."]
        );
        assert_eq!(sentences("Version 2.4 fixes it"), ["Version 2.4 fixes it"]);
        assert_eq!(
            sentences("and then we're going to"),
            ["and then we're going to"]
        );
        assert!(sentences("   ").is_empty());
        assert_eq!(sentences("¿Qué? ¡Sí! Ok."), ["¿Qué?", "¡Sí!", "Ok."]);
    }

    #[test]
    fn decode_cap_grows_then_saturates() {
        assert_eq!(max_decoding_length(0), 16);
        assert_eq!(max_decoding_length(10), 56);
        assert_eq!(max_decoding_length(usize::MAX), 256);
    }
}
