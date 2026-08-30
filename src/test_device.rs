//! An in-memory [`BlockRead`] for tests.
//!
//! # Why one is enough
//!
//! This type was declared **seven times** in this crate — six inside
//! `src/` and once in `tests/common/mod.rs` — with bodies that differed
//! only in whether the buffer sat behind a `Mutex`, and in whether the
//! locals were called `start`/`end` or `s`/`e`.
//!
//! Test scaffolding duplicates more readily than production code
//! precisely because nobody is looking for it: each module needed a
//! device, writing twenty lines was quicker than finding the one next
//! door, and no reviewer notices a helper that only tests use. The cost
//! is the same as anywhere else — seven chances for the short-read
//! behaviour to differ, which is the one behaviour a device under test
//! is being asked about.
//!
//! # The one that stays
//!
//! `tests/common/mod.rs` keeps its own copy. Integration tests compile
//! as a separate crate and cannot see a `#[cfg(test)]` item, so sharing
//! with them would mean exporting this from the public API behind a
//! feature — a change to what the crate offers the world, made for the
//! convenience of its own tests. Two copies across a compilation
//! boundary is a real boundary; seven inside one module tree was not.

use fs_core::{BlockRead, Result as BlockResult};
use std::sync::Mutex;

/// A byte buffer that answers reads like a device.
///
/// Behind a `Mutex` so it satisfies `Sync`, which `Arc<dyn BlockRead>`
/// requires — not because anything here writes to it.
pub(crate) struct MemDev(pub Mutex<Vec<u8>>);

impl MemDev {
    /// A device holding `bytes`.
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        MemDev(Mutex::new(bytes))
    }
}

impl BlockRead for MemDev {
    /// Reads that run past the end fail as [`fs_core::Error::ShortRead`]
    /// rather than being padded.
    ///
    /// A device that quietly zero-fills makes a parser reading past its
    /// structure look correct, which is the opposite of what a test
    /// device is for.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> BlockResult<()> {
        let v = self.0.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if end > v.len() {
            return Err(fs_core::Error::ShortRead {
                offset,
                want: buf.len(),
                got: v.len().saturating_sub(start),
            });
        }
        buf.copy_from_slice(&v[start..end]);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read past the end is refused, and says how much it got.
    ///
    /// This is the behaviour the seven copies each restated, and the
    /// only one a parser under test can actually observe.
    #[test]
    fn a_read_past_the_end_is_a_short_read_not_a_zero_fill() {
        let dev = MemDev::new(vec![0xAB; 8]);
        let mut buf = [0u8; 16];
        let err = dev.read_at(0, &mut buf).expect_err("past the end");
        match err {
            fs_core::Error::ShortRead { offset, want, got } => {
                assert_eq!((offset, want, got), (0, 16, 8));
            }
            other => panic!("expected ShortRead, got {other:?}"),
        }
        assert_eq!(buf, [0u8; 16], "a refused read leaves the buffer alone");
    }

    /// An offset past the end reports zero available, not a wrapped
    /// subtraction.
    #[test]
    fn an_offset_past_the_end_reports_nothing_available() {
        let dev = MemDev::new(vec![0xAB; 8]);
        let mut buf = [0u8; 4];
        match dev.read_at(100, &mut buf).expect_err("past the end") {
            fs_core::Error::ShortRead { got, .. } => assert_eq!(got, 0),
            other => panic!("expected ShortRead, got {other:?}"),
        }
    }

    #[test]
    fn a_read_inside_the_buffer_returns_those_bytes() {
        let dev = MemDev::new((0u8..16).collect());
        let mut buf = [0u8; 4];
        dev.read_at(4, &mut buf).expect("inside");
        assert_eq!(buf, [4, 5, 6, 7]);
        assert_eq!(dev.size_bytes(), 16);
    }
}
