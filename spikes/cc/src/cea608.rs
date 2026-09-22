//! CEA-608 (line 21) caption encoder: roll-up, pop-on and paint-on on CC1–CC4.
//!
//! The encoder turns text into a queue of byte pairs (odd parity already set).
//! The caller drains one pair per field per frame with [`Cc608Encoder::next_pair`];
//! [`crate::CcMux`] does that pacing for you.

use crate::text::{self, Cell};
use crate::{Channel, odd_parity};
use std::collections::VecDeque;

/// Characters per caption row.
pub const COLUMNS: usize = 32;

/// Maximum queued byte pairs per encoder (~30 s at 30 fps). When exceeded, the
/// oldest whole lines are dropped so live captions never fall further behind.
pub const MAX_QUEUE_PAIRS: usize = 900;

/// The null pair (0x00 0x00 with parity): padding that every decoder ignores.
pub const NULL_PAIR: [u8; 2] = [0x80, 0x80];

/// Caption display style (CEA-608 §B/C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode608 {
    /// Roll-up with 2, 3 or 4 visible rows (RU2/RU3/RU4). Other values are clamped.
    RollUp(u8),
    /// Pop-on: build the caption off screen (RCL/ENM), then swap it in (EOC).
    PopOn,
    /// Paint-on: characters appear as they arrive (RDC).
    PaintOn,
}

/// Miscellaneous control codes (second byte; first byte depends on channel).
mod misc {
    pub const RCL: u8 = 0x20;
    pub const BS: u8 = 0x21;
    pub const DER: u8 = 0x24;
    pub const RU2: u8 = 0x25;
    pub const RDC: u8 = 0x29;
    pub const EDM: u8 = 0x2C;
    pub const CR: u8 = 0x2D;
    pub const ENM: u8 = 0x2E;
    pub const EOC: u8 = 0x2F;
}

impl Channel {
    /// First byte of miscellaneous control codes (RCL, CR, EOC, ...): CC1 0x14,
    /// CC2 0x1C, CC3 0x15, CC4 0x1D.
    pub fn misc_control_byte(self) -> u8 {
        match self {
            Channel::Cc1 => 0x14,
            Channel::Cc2 => 0x1C,
            Channel::Cc3 => 0x15,
            Channel::Cc4 => 0x1D,
        }
    }

    /// True for the second data channel of a field (CC2, CC4): control codes carry bit 0x08.
    pub fn is_second_in_field(self) -> bool {
        matches!(self, Channel::Cc2 | Channel::Cc4)
    }

    /// The field that carries this channel.
    pub fn field(self) -> crate::CcType {
        match self {
            Channel::Cc1 | Channel::Cc2 => crate::CcType::Field1,
            Channel::Cc3 | Channel::Cc4 => crate::CcType::Field2,
        }
    }

    fn chan_bit(self) -> u8 {
        if self.is_second_in_field() { 0x08 } else { 0 }
    }
}

/// Preamble address code for `row` (1–15) with white text indented `indent`
/// columns (rounded down to a multiple of 4, max 28). Parity not applied.
pub fn pac(channel: Channel, row: u8, indent: u8) -> [u8; 2] {
    // (first byte, second-byte base) per row, CEA-608 table 53.
    const ROWS: [(u8, u8); 15] = [
        (0x11, 0x40),
        (0x11, 0x60),
        (0x12, 0x40),
        (0x12, 0x60),
        (0x15, 0x40),
        (0x15, 0x60),
        (0x16, 0x40),
        (0x16, 0x60),
        (0x17, 0x40),
        (0x17, 0x60),
        (0x10, 0x40),
        (0x13, 0x40),
        (0x13, 0x60),
        (0x14, 0x40),
        (0x14, 0x60),
    ];
    let (hi, base) = ROWS[usize::from(row.clamp(1, 15) - 1)];
    let indent_code = (indent.min(28) / 4) << 1;
    [hi | channel.chan_bit(), base | 0x10 | indent_code]
}

/// One queued pair. `boundary` marks where another channel on the same field
/// may take over (the pair starts with a control code for this channel).
#[derive(Clone, Copy, Debug)]
struct Pair {
    bytes: [u8; 2],
    boundary: bool,
}

/// CEA-608 encoder for one channel.
///
/// Text is queued with [`push_text`](Self::push_text); every video frame the
/// caller asks for this field's byte pair with [`next_pair`](Self::next_pair),
/// which returns `None` when idle (send [`NULL_PAIR`] then). Pairs already carry
/// parity. Control codes are sent twice, as CEA-608 recommends. Text is wrapped
/// at 32 columns; unsupported characters are transliterated (`œ` → `oe`) or
/// dropped (emoji, CJK).
#[derive(Clone, Debug)]
pub struct Cc608Encoder {
    pub channel: Channel,
    mode: Mode608,
    base_row: u8,
    popon_rows: u8,
    queue: VecDeque<Pair>,
    last_queued: Option<[u8; 2]>,
    dropped_pairs: u64,
}

impl Cc608Encoder {
    /// Roll-up, 3 rows, bottom row 15.
    pub fn new(channel: Channel) -> Self {
        Self::with_mode(channel, Mode608::RollUp(3))
    }

    pub fn with_mode(channel: Channel, mode: Mode608) -> Self {
        Self {
            channel,
            mode: normalize(mode),
            base_row: 15,
            popon_rows: 2,
            queue: VecDeque::new(),
            last_queued: None,
            dropped_pairs: 0,
        }
    }

    pub fn mode(&self) -> Mode608 {
        self.mode
    }

    /// Changes mode for text pushed from now on (the mode code is resent with every line).
    pub fn set_mode(&mut self, mode: Mode608) {
        self.mode = normalize(mode);
    }

    /// Bottom caption row (1–15, default 15). Clamped so roll-up/pop-on rows fit.
    pub fn set_base_row(&mut self, row: u8) {
        self.base_row = row.clamp(4, 15);
    }

    /// Max rows per pop-on/paint-on caption (1–4, default 2).
    pub fn set_popon_rows(&mut self, rows: u8) {
        self.popon_rows = rows.clamp(1, 4);
    }

    /// Queues `text` as one caption. `\n` forces a line break; long text wraps
    /// at word boundaries (hard-split if a word exceeds 32 columns). Empty or
    /// all-unsupported text queues nothing.
    pub fn push_text(&mut self, text: &str) {
        let lines = text::wrap(&text::cells_608(text), COLUMNS);
        if lines.is_empty() {
            return;
        }
        match self.mode {
            Mode608::RollUp(rows) => {
                for line in &lines {
                    self.control(misc::RU2 + rows - 2, true);
                    self.control(misc::CR, false);
                    self.pac_twice(self.base_row, 0);
                    self.text_line(line);
                }
            }
            Mode608::PopOn => {
                for group in lines.chunks(usize::from(self.popon_rows)) {
                    self.control(misc::RCL, true);
                    self.control(misc::ENM, false);
                    self.rows(group);
                    self.control(misc::EOC, false);
                }
            }
            Mode608::PaintOn => {
                for group in lines.chunks(usize::from(self.popon_rows)) {
                    self.control(misc::RDC, true);
                    self.control(misc::EDM, false);
                    self.rows(group);
                }
            }
        }
        self.trim();
    }

    /// Erases the displayed caption (EDM), and in pop-on mode the buffer too (ENM).
    pub fn clear(&mut self) {
        self.control(misc::EDM, true);
        if self.mode == Mode608::PopOn {
            self.control(misc::ENM, false);
        }
    }

    /// Deletes the character left of the cursor (BS).
    pub fn backspace(&mut self) {
        self.control(misc::BS, true);
    }

    /// Deletes from the cursor to the end of the row (DER).
    pub fn delete_to_end_of_row(&mut self) {
        self.control(misc::DER, true);
    }

    /// Next byte pair for this channel's field, or `None` when idle.
    pub fn next_pair(&mut self) -> Option<[u8; 2]> {
        self.queue.pop_front().map(|p| p.bytes)
    }

    /// True when idle or when the next pair starts a new line, i.e. another
    /// channel on the same field may be interleaved here.
    pub fn at_boundary(&self) -> bool {
        self.queue.front().is_none_or(|p| p.boundary)
    }

    /// Queued pairs not yet sent.
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Pairs discarded because the queue exceeded [`MAX_QUEUE_PAIRS`].
    pub fn dropped_pairs(&self) -> u64 {
        self.dropped_pairs
    }

    fn rows(&mut self, group: &[Vec<Cell>]) {
        let n = group.len() as u8; // at most 4
        for (i, line) in group.iter().enumerate() {
            self.pac_twice(self.base_row + 1 - n + i as u8, 0);
            self.text_line(line);
        }
    }

    fn control(&mut self, lo: u8, boundary: bool) {
        self.twice([self.channel.misc_control_byte(), lo], boundary);
    }

    fn pac_twice(&mut self, row: u8, indent: u8) {
        self.twice(pac(self.channel, row, indent), false);
    }

    /// Queues a control-type pair twice. If the same pair was just queued
    /// (e.g. "èè"), a doubled DER goes between: FFmpeg and libcaption ignore
    /// *every* repeat of the last code until a different non-padding code
    /// arrives (a null pair does not reset them). DER at the write position
    /// deletes nothing because we always write at the end of the row.
    fn twice(&mut self, raw: [u8; 2], boundary: bool) {
        let p = [odd_parity(raw[0]), odd_parity(raw[1])];
        if self.last_queued == Some(p) {
            let der = [
                odd_parity(self.channel.misc_control_byte()),
                odd_parity(misc::DER),
            ];
            self.queue.push_back(Pair {
                bytes: der,
                boundary: false,
            });
            self.queue.push_back(Pair {
                bytes: der,
                boundary: false,
            });
        }
        self.queue.push_back(Pair { bytes: p, boundary });
        self.queue.push_back(Pair {
            bytes: p,
            boundary: false,
        });
        self.last_queued = Some(p);
    }

    fn standard(&mut self, a: u8, b: u8) {
        let p = [odd_parity(a), odd_parity(b)];
        self.queue.push_back(Pair {
            bytes: p,
            boundary: false,
        });
        self.last_queued = Some(p);
    }

    fn text_line(&mut self, line: &[Cell]) {
        let chan = self.channel.chan_bit();
        let mut pending: Option<u8> = None;
        for cell in line {
            match *cell {
                Cell::Basic(c) => match pending.take() {
                    Some(p) => self.standard(p, c),
                    None => pending = Some(c),
                },
                Cell::Special(lo) => {
                    if let Some(p) = pending.take() {
                        self.standard(p, 0);
                    }
                    self.twice([0x11 | chan, lo], false);
                }
                Cell::Extended { set, lo, base } => {
                    // Base character first; the extended code backspaces over it
                    // on decoders that know the extended set.
                    match pending.take() {
                        Some(p) => self.standard(p, base),
                        None => self.standard(base, 0),
                    }
                    self.twice([set | chan, lo], false);
                }
            }
        }
        if let Some(p) = pending {
            self.standard(p, 0);
        }
    }

    /// Drops the oldest whole lines until the queue fits.
    fn trim(&mut self) {
        while self.queue.len() > MAX_QUEUE_PAIRS {
            self.queue.pop_front();
            self.dropped_pairs += 1;
            while self.queue.front().is_some_and(|p| !p.boundary) {
                self.queue.pop_front();
                self.dropped_pairs += 1;
            }
        }
    }
}

fn normalize(mode: Mode608) -> Mode608 {
    match mode {
        Mode608::RollUp(n) => Mode608::RollUp(n.clamp(2, 4)),
        m => m,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(e: &mut Cc608Encoder) -> Vec<[u8; 2]> {
        std::iter::from_fn(|| e.next_pair()).collect()
    }

    fn strip(pairs: &[[u8; 2]]) -> Vec<[u8; 2]> {
        pairs.iter().map(|p| [p[0] & 0x7F, p[1] & 0x7F]).collect()
    }

    #[test]
    fn rollup_line_layout() {
        let mut e = Cc608Encoder::with_mode(Channel::Cc1, Mode608::RollUp(2));
        e.push_text("HI!");
        let p = strip(&drain(&mut e));
        assert_eq!(
            p,
            [
                [0x14, 0x25],
                [0x14, 0x25],
                [0x14, 0x2D],
                [0x14, 0x2D],
                [0x14, 0x70],
                [0x14, 0x70],
                *b"HI",
                [b'!', 0]
            ]
        );
    }

    #[test]
    fn channel_control_bytes() {
        for (ch, hi, pac_hi) in [
            (Channel::Cc1, 0x14, 0x14),
            (Channel::Cc2, 0x1C, 0x1C),
            (Channel::Cc3, 0x15, 0x14),
            (Channel::Cc4, 0x1D, 0x1C),
        ] {
            let mut e = Cc608Encoder::with_mode(ch, Mode608::RollUp(4));
            e.push_text("A");
            let p = strip(&drain(&mut e));
            assert_eq!(p[0], [hi, 0x27], "{ch:?}");
            assert_eq!(p[4], [pac_hi, 0x70], "{ch:?}");
        }
    }

    #[test]
    fn pac_rows_and_indent() {
        assert_eq!(pac(Channel::Cc1, 1, 0), [0x11, 0x50]);
        assert_eq!(pac(Channel::Cc1, 11, 0), [0x10, 0x50]);
        assert_eq!(pac(Channel::Cc1, 15, 8), [0x14, 0x74]);
        assert_eq!(pac(Channel::Cc2, 14, 28), [0x1C, 0x5E]);
    }

    #[test]
    fn popon_sequence() {
        let mut e = Cc608Encoder::with_mode(Channel::Cc1, Mode608::PopOn);
        e.push_text("AB");
        let p = strip(&drain(&mut e));
        assert_eq!(
            p,
            [
                [0x14, 0x20],
                [0x14, 0x20],
                [0x14, 0x2E],
                [0x14, 0x2E],
                [0x14, 0x70],
                [0x14, 0x70],
                *b"AB",
                [0x14, 0x2F],
                [0x14, 0x2F]
            ]
        );
    }

    #[test]
    fn painton_uses_rdc() {
        let mut e = Cc608Encoder::with_mode(Channel::Cc3, Mode608::PaintOn);
        e.push_text("X");
        let p = strip(&drain(&mut e));
        assert_eq!(p[0], [0x15, 0x29]);
    }

    #[test]
    fn accented_characters() {
        let mut e = Cc608Encoder::with_mode(Channel::Cc1, Mode608::RollUp(2));
        e.push_text("é à Ä");
        let p = strip(&drain(&mut e));
        // é is basic 0x5C; à special 11 38 twice; Ä is 'A' then 13 30 twice.
        assert_eq!(
            &p[6..],
            [
                [0x5C, b' '],
                [0x11, 0x38],
                [0x11, 0x38],
                *b" A",
                [0x13, 0x30],
                [0x13, 0x30]
            ]
        );
    }

    #[test]
    fn repeated_special_gets_separator() {
        let mut e = Cc608Encoder::with_mode(Channel::Cc2, Mode608::RollUp(2));
        e.push_text("èè");
        let p = strip(&drain(&mut e));
        assert_eq!(
            &p[6..],
            [
                [0x19, 0x3A],
                [0x19, 0x3A],
                [0x1C, 0x24],
                [0x1C, 0x24],
                [0x19, 0x3A],
                [0x19, 0x3A]
            ]
        );
    }

    #[test]
    fn wraps_at_32_columns() {
        let mut e = Cc608Encoder::with_mode(Channel::Cc1, Mode608::RollUp(3));
        let text = "word ".repeat(20);
        e.push_text(&text);
        let p = strip(&drain(&mut e));
        let lines: Vec<usize> = p
            .split(|x| *x == [0x14, 0x26])
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.iter()
                    .skip(5)
                    .map(|x| (x[0] >= 0x20) as usize + (x[1] >= 0x20) as usize)
                    .sum()
            })
            .collect();
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|&n| n <= COLUMNS), "{lines:?}");
    }

    #[test]
    fn weird_text_never_panics() {
        let long = "x".repeat(10_000);
        let inputs = [
            "",
            " ",
            "\n\n\n",
            "\u{0}\u{7}\t\r",
            "😀🎉",
            "日本語のテキスト",
            "Привет мир",
            "مرحبا",
            &long,
            "a\u{301}",
        ];
        for t in inputs {
            for mode in [Mode608::RollUp(9), Mode608::PopOn, Mode608::PaintOn] {
                let mut e = Cc608Encoder::with_mode(Channel::Cc4, mode);
                e.push_text(t);
                assert!(e.pending() <= MAX_QUEUE_PAIRS);
                for p in drain(&mut e) {
                    assert_eq!(p[0].count_ones() % 2, 1);
                    assert_eq!(p[1].count_ones() % 2, 1);
                }
            }
        }
        let mut e = Cc608Encoder::new(Channel::Cc1);
        e.push_text("😀");
        assert_eq!(e.next_pair(), None);
    }

    #[test]
    fn overflow_drops_oldest_lines() {
        let mut e = Cc608Encoder::new(Channel::Cc1);
        for _ in 0..200 {
            e.push_text("THIS LINE IS FAIRLY LONG TEXT OK");
        }
        assert!(e.pending() <= MAX_QUEUE_PAIRS);
        assert!(e.dropped_pairs() > 0);
        assert!(e.at_boundary());
    }
}
