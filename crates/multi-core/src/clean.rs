//! Caption text cleaning: valid UTF-8, no control characters, and ASCII
//! stand-ins for symbols the GStreamer caption encoders would turn into a
//! space (S2b finding 4: unmapped characters become a space, and `©`/`®`
//! are not mapped). Runs before the word filter and before text reaches the
//! caption lanes.

/// Cleans text from a worker: see [`clean`]. Invalid UTF-8 is dropped.
pub fn clean_bytes(bytes: &[u8]) -> String {
    clean(&String::from_utf8_lossy(bytes))
}

/// ASCII replacement for a symbol, if it has one.
fn transliterate(c: char) -> Option<&'static str> {
    Some(match c {
        '©' => "(c)",
        '®' => "(R)",
        '™' => "TM",
        '…' => "...",
        '€' => "EUR",
        'œ' => "oe",
        'Œ' => "OE",
        '‘' | '’' | '‚' | '‛' | '′' | '`' | '´' => "'",
        '“' | '”' | '„' | '‟' | '″' => "\"",
        '‐' | '‑' | '‒' | '–' | '—' | '―' | '−' => "-",
        '•' | '·' => "-",
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => " ",
        _ => return None,
    })
}

/// Characters removed outright: emoji, pictographs, dingbats, variation
/// selectors, zero-width and bidi controls, private use, the replacement
/// character.
fn removed(c: char) -> bool {
    matches!(c,
        '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{2190}'..='\u{21FF}'
        | '\u{2300}'..='\u{23FF}'
        | '\u{2460}'..='\u{27BF}'
        | '\u{2900}'..='\u{2BFF}'
        | '\u{E000}'..='\u{F8FF}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFFD}'
        | '\u{1F000}'..='\u{1FAFF}'
        | '\u{E0000}'..='\u{E007F}'
        | '\u{F0000}'..='\u{10FFFF}')
}

/// Strips control characters and emoji, transliterates symbols, and
/// collapses runs of whitespace to one space (trimmed).
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let push = |s: &str, out: &mut String| {
        for c in s.chars() {
            if c == ' ' {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            } else {
                out.push(c);
            }
        }
    };
    let mut buf = [0u8; 4];
    for c in text.chars() {
        if let Some(r) = transliterate(c) {
            push(r, &mut out);
        } else if c.is_whitespace() {
            push(" ", &mut out);
        } else if !c.is_control() && !removed(c) {
            push(c.encode_utf8(&mut buf), &mut out);
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transliterates_symbols() {
        assert_eq!(clean("© 2026 Acme®"), "(c) 2026 Acme(R)");
        assert_eq!(clean("Wait…"), "Wait...");
        assert_eq!(clean("5 €"), "5 EUR");
        assert_eq!(clean("cœur Œuvre"), "coeur OEuvre");
        assert_eq!(clean("“It’s” — ok – fine"), "\"It's\" - ok - fine");
    }

    #[test]
    fn strips_controls_and_emoji() {
        assert_eq!(clean("a\u{0}b\u{7}c\td\ne"), "abc d e");
        assert_eq!(clean("great 👍🏽 job 🖕 ❤️ ok"), "great job ok");
        assert_eq!(clean("zero\u{200B}width"), "zerowidth");
        assert_eq!(clean("  lots   of\u{00A0}space  "), "lots of space");
    }

    #[test]
    fn keeps_caption_letters() {
        let s = "¿Qué pasó? Ça va, Müller! ñ ß ¡sí! «oui» ½ °";
        assert_eq!(clean(s), s);
    }

    #[test]
    fn invalid_utf8_is_dropped() {
        assert_eq!(clean_bytes(b"ok \xff\xfe fine"), "ok fine");
        assert_eq!(clean_bytes(b""), "");
    }
}
