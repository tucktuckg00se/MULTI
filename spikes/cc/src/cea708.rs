//! CEA-708 (DTVCC) service-layer encoder: one roll-up window per service.
//!
//! [`Cc708Encoder`] produces service-layer commands and text as atomic units
//! (a command or character is never split). [`crate::CcMux`] packs units from
//! several services into service blocks and DTVCC packets.
//!
//! Only G0 (ASCII), G1 (Latin-1) and G2/G3 (via EXT1) are emitted. CJK,
//! Arabic, Cyrillic etc. would need the `P16` code (0x18 + 2 bytes), whose
//! character mapping CEA-708 leaves to the region (e.g. KS X 1001 in Korea);
//! decoders only render it when told the encoding out of band.

use crate::text;
use std::collections::VecDeque;

/// Highest standard service number (services 1–6 use the short block header).
pub const MAX_SERVICE: u8 = 6;

/// Maximum queued units per service before the oldest lines are dropped.
pub const MAX_QUEUE_UNITS: usize = 4000;

/// Lines between repeats of the window definition, so a receiver that tunes in
/// mid-stream gets a window within a few lines.
const REDEFINE_EVERY: u32 = 4;

/// C0 and C1 command codes used here (CEA-708 §7.1.4, §7.1.5).
pub mod cmd {
    pub const ETX: u8 = 0x03;
    pub const BS: u8 = 0x08;
    pub const CR: u8 = 0x0D;
    pub const EXT1: u8 = 0x10;
    pub const P16: u8 = 0x18;
    pub const CW0: u8 = 0x80;
    pub const CLW: u8 = 0x88;
    pub const DSW: u8 = 0x89;
    pub const HDW: u8 = 0x8A;
    pub const DLW: u8 = 0x8C;
    pub const RST: u8 = 0x8F;
    pub const SPA: u8 = 0x90;
    pub const SPC: u8 = 0x91;
    pub const SPL: u8 = 0x92;
    pub const SWA: u8 = 0x97;
    pub const DF0: u8 = 0x98;
}

/// A service-layer unit: one command with its parameters, or one character.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Unit {
    bytes: Vec<u8>,
    /// First unit of a line; the queue is trimmed at these.
    line_start: bool,
}

/// CEA-708 encoder for one caption service (1–6), writing roll-up captions
/// into window 0.
///
/// Every few lines it re-sends DefineWindow + SetWindowAttributes +
/// SetPenAttributes + SetPenColor so late joiners get a window. Each line is
/// `CR` + text + `ETX`.
#[derive(Clone, Debug)]
pub struct Cc708Encoder {
    service: u8,
    rows: u8,
    columns: u8,
    queue: VecDeque<Unit>,
    lines_since_define: u32,
    dropped_units: u64,
}

impl Cc708Encoder {
    /// Encoder for `service` (1–6); `None` for any other number. Defaults to
    /// 3 rows of 32 columns.
    pub fn new(service: u8) -> Option<Self> {
        (1..=MAX_SERVICE).contains(&service).then(|| Self {
            service,
            rows: 3,
            columns: 32,
            queue: VecDeque::new(),
            lines_since_define: REDEFINE_EVERY,
            dropped_units: 0,
        })
    }

    pub fn service(&self) -> u8 {
        self.service
    }

    /// Window size: rows 1–12, columns 1–42 (32 for 4:3 safe area). Takes effect
    /// at the next window definition.
    pub fn set_window(&mut self, rows: u8, columns: u8) {
        self.rows = rows.clamp(1, 12);
        self.columns = columns.clamp(1, 42);
        self.lines_since_define = REDEFINE_EVERY;
    }

    /// Queues `text` as roll-up lines (wrapped at the window width; `\n` breaks).
    pub fn push_text(&mut self, text: &str) {
        let lines = text::wrap(&text::cells_708(text), usize::from(self.columns));
        for line in lines {
            let mut first = true;
            if self.lines_since_define >= REDEFINE_EVERY {
                for u in self.window_definition() {
                    self.push(u, first);
                    first = false;
                }
                self.lines_since_define = 0;
            }
            self.push(vec![cmd::CR], first);
            for cell in line {
                self.push(cell, false);
            }
            self.push(vec![cmd::ETX], false);
            self.lines_since_define += 1;
        }
        self.trim();
    }

    /// Clears window 0 (ClearWindows with bitmap 0x01).
    pub fn clear(&mut self) {
        self.push(vec![cmd::CLW, 0x01], true);
    }

    /// Deletes the character before the pen (BS).
    pub fn backspace(&mut self) {
        self.push(vec![cmd::BS], false);
    }

    /// Queued bytes not yet packed.
    pub fn pending_bytes(&self) -> usize {
        self.queue.iter().map(|u| u.bytes.len()).sum()
    }

    pub fn is_idle(&self) -> bool {
        self.queue.is_empty()
    }

    /// Units discarded because the queue exceeded [`MAX_QUEUE_UNITS`].
    pub fn dropped_units(&self) -> u64 {
        self.dropped_units
    }

    /// Takes whole units totalling at most `max` bytes (service-block payload).
    pub(crate) fn take_block(&mut self, max: usize) -> Vec<u8> {
        let mut block = Vec::new();
        while let Some(u) = self.queue.front() {
            if u.bytes.len() > 31 {
                // Can never fit a service block; drop rather than stall.
                self.queue.pop_front();
                self.dropped_units += 1;
                continue;
            }
            if block.len() + u.bytes.len() > max {
                break;
            }
            if let Some(u) = self.queue.pop_front() {
                block.extend_from_slice(&u.bytes);
            }
        }
        block
    }

    fn push(&mut self, bytes: Vec<u8>, line_start: bool) {
        self.queue.push_back(Unit { bytes, line_start });
    }

    fn trim(&mut self) {
        while self.queue.len() > MAX_QUEUE_UNITS {
            self.queue.pop_front();
            self.dropped_units += 1;
            while self.queue.front().is_some_and(|u| !u.line_start) {
                self.queue.pop_front();
                self.dropped_units += 1;
            }
            // The window definition may have been dropped; resend it next line.
            self.lines_since_define = REDEFINE_EVERY;
        }
    }

    /// DefineWindow 0 (visible, bottom-centre, roll-up style) + attributes.
    fn window_definition(&self) -> [Vec<u8>; 4] {
        // DF0: 0 0 v rl cl p2 p1 p0 | rp av(7) | ah(8) | ap(4) rc(4) | 0 0 cc(6) | 0 0 ws(3) ps(3)
        let visible = 1 << 5;
        let priority = 0;
        let df = vec![
            cmd::DF0,
            visible | priority,
            0x80 | 90,                  // relative positioning, 90% down
            50,                         // 50% across
            (7 << 4) | (self.rows - 1), // anchor bottom-centre, row count
            self.columns - 1,           // column count
            (4 << 3) | 1,               // window style 4 (NTSC roll-up), pen style 1
        ];
        // SWA: fill solid black; no border; left-justify, print L→R, scroll bottom→top; snap.
        let swa = vec![cmd::SWA, 0x00, 0x00, 3 << 2, 0x00];
        // SPA: standard size, normal offset, no text tag; font style 0, no edge/italic/underline.
        let spa = vec![cmd::SPA, (1 << 2) | 1, 0x00];
        // SPC: white on solid black, black edge.
        let spc = vec![cmd::SPC, 0x3F, 0x00, 0x00];
        [df, swa, spa, spc]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(e: &mut Cc708Encoder) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = e.take_block(31);
            if b.is_empty() {
                break;
            }
            out.extend(b);
        }
        out
    }

    #[test]
    fn services_range() {
        assert!(Cc708Encoder::new(0).is_none());
        assert!(Cc708Encoder::new(7).is_none());
        assert!((1..=6).all(|s| Cc708Encoder::new(s).is_some()));
    }

    #[test]
    fn first_line_defines_window() {
        let mut e = Cc708Encoder::new(1).unwrap_or_else(|| unreachable!());
        e.push_text("Hé Œ");
        let b = drain(&mut e);
        assert_eq!(&b[..7], [0x98, 0x20, 0xDA, 50, 0x72, 31, 0x21]);
        assert_eq!(&b[7..12], [0x97, 0, 0, 0x0C, 0]);
        assert_eq!(
            &b[b.len() - 7..],
            [0x0D, b'H', 0xE9, b' ', 0x10, 0x2C, 0x03]
        );
    }

    #[test]
    fn blocks_never_split_units() {
        let mut e = Cc708Encoder::new(2).unwrap_or_else(|| unreachable!());
        e.push_text(&"Œ".repeat(40));
        loop {
            let b = e.take_block(7);
            if b.is_empty() {
                break;
            }
            assert!(b.len() <= 7);
        }
        assert!(e.is_idle());
    }

    #[test]
    fn weird_text_never_panics() {
        let long = "y".repeat(100_000);
        for t in ["", "\u{0}\u{1b}", "😀", "漢字", &long, "\n"] {
            let mut e = Cc708Encoder::new(6).unwrap_or_else(|| unreachable!());
            e.push_text(t);
            assert!(e.queue.len() <= MAX_QUEUE_UNITS);
            drain(&mut e);
        }
    }
}
