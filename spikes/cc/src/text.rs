//! Text preparation shared by the 608 and 708 encoders: character-set mapping,
//! transliteration fallback and word wrapping.

/// One displayed character cell in CEA-608.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    /// Standard character (one byte, 0x20–0x7F, 608 meaning).
    Basic(u8),
    /// Special North American character: `0x11 lo` (lo 0x30–0x3F).
    Special(u8),
    /// Extended character: `set lo` (set 0x12 or 0x13), sent after `base`,
    /// which it overwrites on decoders that support it.
    Extended { set: u8, lo: u8, base: u8 },
}

/// A cell in a 708 line: the bytes that encode one character (G0/G1 one
/// byte, G2/G3 `EXT1 xx`).
pub type Cell708 = Vec<u8>;

/// What wrapping needs to know about a cell.
pub trait Glyph {
    fn is_space(&self) -> bool;
    fn space() -> Self;
}

impl Glyph for Cell {
    fn is_space(&self) -> bool {
        *self == Cell::Basic(b' ')
    }
    fn space() -> Self {
        Cell::Basic(b' ')
    }
}

impl Glyph for Cell708 {
    fn is_space(&self) -> bool {
        self.as_slice() == b" "
    }
    fn space() -> Self {
        vec![b' ']
    }
}

/// Hard line break marker inside a cell sequence.
pub enum Item<G> {
    Cell(G),
    Break,
}

/// Maps a character to 608 cells. Unsupported letters with diacritics fall
/// back to the plain letter; anything else unsupported is dropped.
fn map_608(c: char, out: &mut Vec<Item<Cell>>) {
    use Cell::*;
    let basic = |b: u8| Basic(b);
    let ext = |set: u8, lo: u8, base: u8| Extended { set, lo, base };
    let cell = match c {
        // ASCII positions that 608 redefines are sent via the extended sets.
        '*' => ext(0x12, 0x28, b'.'),
        '\\' => ext(0x13, 0x2B, b'/'),
        '^' => ext(0x13, 0x2C, b' '),
        '_' => ext(0x13, 0x2D, b'-'),
        '`' => basic(b'\''),
        '{' => ext(0x13, 0x29, b'('),
        '|' => ext(0x13, 0x2E, b'!'),
        '}' => ext(0x13, 0x2A, b')'),
        '~' => ext(0x13, 0x2F, b'-'),
        ' '..='~' => basic(c as u8),
        // Basic set replacements.
        'á' => basic(0x2A),
        'é' => basic(0x5C),
        'í' => basic(0x5E),
        'ó' => basic(0x5F),
        'ú' => basic(0x60),
        'ç' => basic(0x7B),
        '÷' => basic(0x7C),
        'Ñ' => basic(0x7D),
        'ñ' => basic(0x7E),
        '█' => basic(0x7F),
        // Special North American set (0x11 0x30–0x3F).
        '®' => Special(0x30),
        '°' => Special(0x31),
        '½' => Special(0x32),
        '¿' => Special(0x33),
        '™' => Special(0x34),
        '¢' => Special(0x35),
        '£' => Special(0x36),
        '♪' => Special(0x37),
        'à' => Special(0x38),
        'è' => Special(0x3A),
        'â' => Special(0x3B),
        'ê' => Special(0x3C),
        'î' => Special(0x3D),
        'ô' => Special(0x3E),
        'û' => Special(0x3F),
        // Extended Spanish/French/misc (0x12 0x20–0x3F).
        'Á' => ext(0x12, 0x20, b'A'),
        'É' => ext(0x12, 0x21, b'E'),
        'Ó' => ext(0x12, 0x22, b'O'),
        'Ú' => ext(0x12, 0x23, b'U'),
        'Ü' => ext(0x12, 0x24, b'U'),
        'ü' => ext(0x12, 0x25, b'u'),
        '‘' => ext(0x12, 0x26, b'\''),
        '¡' => ext(0x12, 0x27, b'!'),
        '’' => basic(b'\''),
        '—' | '–' => ext(0x12, 0x2A, b'-'),
        '©' => ext(0x12, 0x2B, b'c'),
        '℠' => ext(0x12, 0x2C, b's'),
        '•' => ext(0x12, 0x2D, b'.'),
        '“' => ext(0x12, 0x2E, b'"'),
        '”' => ext(0x12, 0x2F, b'"'),
        'À' => ext(0x12, 0x30, b'A'),
        'Â' => ext(0x12, 0x31, b'A'),
        'Ç' => ext(0x12, 0x32, b'C'),
        'È' => ext(0x12, 0x33, b'E'),
        'Ê' => ext(0x12, 0x34, b'E'),
        'Ë' => ext(0x12, 0x35, b'E'),
        'ë' => ext(0x12, 0x36, b'e'),
        'Î' => ext(0x12, 0x37, b'I'),
        'Ï' => ext(0x12, 0x38, b'I'),
        'ï' => ext(0x12, 0x39, b'i'),
        'Ô' => ext(0x12, 0x3A, b'O'),
        'Ù' => ext(0x12, 0x3B, b'U'),
        'ù' => ext(0x12, 0x3C, b'u'),
        'Û' => ext(0x12, 0x3D, b'U'),
        '«' => ext(0x12, 0x3E, b'"'),
        '»' => ext(0x12, 0x3F, b'"'),
        // Extended Portuguese/German/Danish (0x13 0x20–0x3F).
        'Ã' => ext(0x13, 0x20, b'A'),
        'ã' => ext(0x13, 0x21, b'a'),
        'Í' => ext(0x13, 0x22, b'I'),
        'Ì' => ext(0x13, 0x23, b'I'),
        'ì' => ext(0x13, 0x24, b'i'),
        'Ò' => ext(0x13, 0x25, b'O'),
        'ò' => ext(0x13, 0x26, b'o'),
        'Õ' => ext(0x13, 0x27, b'O'),
        'õ' => ext(0x13, 0x28, b'o'),
        'Ä' => ext(0x13, 0x30, b'A'),
        'ä' => ext(0x13, 0x31, b'a'),
        'Ö' => ext(0x13, 0x32, b'O'),
        'ö' => ext(0x13, 0x33, b'o'),
        'ß' => ext(0x13, 0x34, b's'),
        '¥' => ext(0x13, 0x35, b'Y'),
        '¤' => ext(0x13, 0x36, b'o'),
        '¦' => ext(0x13, 0x37, b'!'),
        'Å' => ext(0x13, 0x38, b'A'),
        'å' => ext(0x13, 0x39, b'a'),
        'Ø' => ext(0x13, 0x3A, b'O'),
        'ø' => ext(0x13, 0x3B, b'o'),
        '┌' => ext(0x13, 0x3C, b'+'),
        '┐' => ext(0x13, 0x3D, b'+'),
        '└' => ext(0x13, 0x3E, b'+'),
        '┘' => ext(0x13, 0x3F, b'+'),
        _ => {
            if let Some(s) = fallback(c) {
                for f in s.chars() {
                    map_608(f, out);
                }
            }
            return;
        }
    };
    out.push(Item::Cell(cell));
}

/// Maps a character to CEA-708 bytes (G0 ASCII, G1 Latin-1, G2/G3 via EXT1).
fn map_708(c: char, out: &mut Vec<Item<Cell708>>) {
    let bytes: Vec<u8> = match c {
        '♪' => vec![0x7F],
        ' '..='~' => vec![c as u8],
        '\u{A0}'..='\u{FF}' => vec![c as u32 as u8],
        _ => match g2(c) {
            Some(b) => vec![0x10, b],
            None => {
                if let Some(s) = fallback(c) {
                    for f in s.chars() {
                        map_708(f, out);
                    }
                }
                return;
            }
        },
    };
    out.push(Item::Cell(bytes));
}

/// CEA-708 G2 set (reached with EXT1 0x10).
fn g2(c: char) -> Option<u8> {
    Some(match c {
        '…' => 0x25,
        'Š' => 0x2A,
        'Œ' => 0x2C,
        '█' => 0x30,
        '‘' => 0x31,
        '’' => 0x32,
        '“' => 0x33,
        '”' => 0x34,
        '•' => 0x35,
        '™' => 0x39,
        'š' => 0x3A,
        'œ' => 0x3C,
        '℠' => 0x3D,
        'Ÿ' => 0x3F,
        '⅛' => 0x76,
        '⅜' => 0x77,
        '⅝' => 0x78,
        '⅞' => 0x79,
        '│' => 0x7A,
        '┐' => 0x7B,
        '└' => 0x7C,
        '─' => 0x7D,
        '┘' => 0x7E,
        '┌' => 0x7F,
        _ => return None,
    })
}

/// Plain-text stand-ins for characters a caption set lacks. Returns `None`
/// for characters to drop (emoji, non-Latin scripts, control codes).
fn fallback(c: char) -> Option<&'static str> {
    Some(match c {
        '\t' | '\r' | '\u{A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{3000}' => " ",
        '…' => "...",
        '–' | '—' | '‐' | '‑' | '−' => "-",
        '‘' | '’' | '‚' | '′' => "'",
        '“' | '”' | '„' | '″' | '«' | '»' => "\"",
        '•' | '·' => ".",
        '€' => "EUR",
        'œ' => "oe",
        'Œ' => "OE",
        'æ' => "ae",
        'Æ' => "AE",
        'ß' => "ss",
        'ÿ' => "y",
        'Ÿ' => "Y",
        'ā' | 'ă' | 'ą' | 'ǎ' | 'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => "a",
        'Ā' | 'Ă' | 'Ą' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => "A",
        'ć' | 'ĉ' | 'ċ' | 'č' | 'ç' => "c",
        'Ć' | 'Ĉ' | 'Ċ' | 'Č' | 'Ç' => "C",
        'ď' | 'đ' => "d",
        'Ď' | 'Đ' => "D",
        'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | 'é' | 'è' | 'ê' | 'ë' => "e",
        'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' | 'É' | 'È' | 'Ê' | 'Ë' => "E",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
        'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => "G",
        'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'í' | 'ì' | 'î' | 'ï' => "i",
        'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' | 'Í' | 'Ì' | 'Î' | 'Ï' => "I",
        'ł' | 'ľ' | 'ĺ' | 'ļ' => "l",
        'Ł' | 'Ľ' | 'Ĺ' | 'Ļ' => "L",
        'ń' | 'ň' | 'ņ' | 'ñ' => "n",
        'Ń' | 'Ň' | 'Ņ' | 'Ñ' => "N",
        'ō' | 'ŏ' | 'ő' | 'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => "o",
        'Ō' | 'Ŏ' | 'Ő' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' | 'Ø' => "O",
        'ŕ' | 'ř' => "r",
        'Ŕ' | 'Ř' => "R",
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => "s",
        'Ś' | 'Ŝ' | 'Ş' | 'Š' | 'Ș' => "S",
        'ţ' | 'ť' | 'ț' => "t",
        'Ţ' | 'Ť' | 'Ț' => "T",
        'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ú' | 'ù' | 'û' | 'ü' => "u",
        'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' | 'Ú' | 'Ù' | 'Û' | 'Ü' => "U",
        'ý' | 'ŷ' => "y",
        'Ý' | 'Ŷ' => "Y",
        'ź' | 'ż' | 'ž' => "z",
        'Ź' | 'Ż' | 'Ž' => "Z",
        _ => return None,
    })
}

fn items<G>(text: &str, map: fn(char, &mut Vec<Item<G>>)) -> Vec<Item<G>> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        if c == '\n' {
            out.push(Item::Break);
        } else {
            map(c, &mut out);
        }
    }
    out
}

/// Text as 608 cells, with `\n` kept as line breaks.
pub fn cells_608(text: &str) -> Vec<Item<Cell>> {
    items(text, map_608)
}

/// Text as 708 cells, with `\n` kept as line breaks.
pub fn cells_708(text: &str) -> Vec<Item<Cell708>> {
    items(text, map_708)
}

/// Greedy word wrap into lines of at most `width` cells. Runs of spaces
/// collapse to one; leading/trailing spaces are dropped; empty lines are
/// removed; words longer than `width` are hard-split.
pub fn wrap<G: Glyph + Clone>(items: &[Item<G>], width: usize) -> Vec<Vec<G>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut paragraph: Vec<Vec<G>> = Vec::new(); // words
    let mut word: Vec<G> = Vec::new();
    let flush_para = |paragraph: &mut Vec<Vec<G>>, lines: &mut Vec<Vec<G>>| {
        let mut line: Vec<G> = Vec::new();
        for w in paragraph.drain(..) {
            let mut w = w.as_slice();
            let sep = usize::from(!line.is_empty());
            if line.len() + sep + w.len() > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            while w.len() > width {
                lines.push(w[..width].to_vec());
                w = &w[width..];
            }
            if !line.is_empty() {
                line.push(G::space());
            }
            line.extend_from_slice(w);
        }
        if !line.is_empty() {
            lines.push(line);
        }
    };
    for item in items {
        match item {
            Item::Cell(g) if g.is_space() => {
                if !word.is_empty() {
                    paragraph.push(std::mem::take(&mut word));
                }
            }
            Item::Cell(g) => word.push(g.clone()),
            Item::Break => {
                if !word.is_empty() {
                    paragraph.push(std::mem::take(&mut word));
                }
                flush_para(&mut paragraph, &mut lines);
            }
        }
    }
    if !word.is_empty() {
        paragraph.push(word);
    }
    flush_para(&mut paragraph, &mut lines);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap_str(s: &str, w: usize) -> Vec<String> {
        wrap(&cells_708(s), w)
            .into_iter()
            .map(|l| l.into_iter().flatten().map(char::from).collect())
            .collect()
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap_str("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap_str("  aa   bb  ", 32), ["aa bb"]);
        assert_eq!(wrap_str("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap_str("a\n\nb", 32), ["a", "b"]);
        assert!(wrap_str("", 32).is_empty());
        assert!(wrap_str("   \n ", 32).is_empty());
    }

    #[test]
    fn every_latin1_letter_maps_in_608() {
        // Every Latin-1 letter used by es/fr/de/pt maps to something printable.
        for c in "áéíóúñÑüÜ¿¡àâçèéêëîïôùûÿœæÀÂÇÈÉÊËÎÏÔÙÛŸŒÆäöüßÄÖÜãõÃÕ".chars()
        {
            let cells = cells_608(&c.to_string());
            assert!(!cells.is_empty(), "{c}");
        }
    }

    #[test]
    fn unsupported_is_dropped() {
        assert!(cells_608("😀日本").is_empty());
        assert!(cells_708("😀日本").is_empty());
    }
}
