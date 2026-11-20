//! The transcript's running line counts: how many laid-out lines come
//! before each block, so a frame finds its first visible block by binary
//! search instead of walking the transcript.
//!
//! [`LayoutCache`] owns:
//! - `prefix[i]`, the laid-out lines of blocks `[0, i)`;
//! - `clean`, the first block whose sum is stale: blocks `[0, clean)` have
//!   a layout at `width` that `prefix` sums, and any change to a block
//!   lowers it to that block ([`LayoutCache::touch`]);
//! - `width`, the width the clean blocks were laid out at: another width
//!   makes every block stale.
//!
//! Each block's own wrapped lines stay with the block (its entry's
//! layouts); this type holds only what sums them.

/// The running line counts of the transcript's blocks (see the module
/// doc).
#[derive(Debug, Default)]
pub(super) struct LayoutCache {
    /// `prefix[i]`: the laid-out lines of blocks `[0, i)`, valid for
    /// `i <= clean`.
    prefix: Vec<usize>,
    /// Blocks `[0, clean)` have a layout at `width` that `prefix` sums.
    clean: usize,
    /// The width the clean blocks were laid out at.
    width: u16,
    /// Blocks laid out so far (tests).
    #[cfg(test)]
    pub(super) laid_out: usize,
    /// Blocks visited to find and copy visible lines (tests).
    #[cfg(test)]
    pub(super) walked: std::cell::Cell<usize>,
}

impl LayoutCache {
    /// Block `index` (or one before it) changed: the clean prefix ends
    /// there.
    pub(super) fn touch(&mut self, index: usize) {
        self.clean = self.clean.min(index);
    }

    /// Every block is stale (e.g. on resize).
    pub(super) fn invalidate_all(&mut self) {
        self.clean = 0;
    }

    /// The first of `len` blocks [`Self::begin`] at `width` would lay out,
    /// without starting.
    pub(super) fn stale_from(&self, width: u16, len: usize) -> usize {
        if width == self.width {
            self.clean.min(len)
        } else {
            0
        }
    }

    /// Start laying out `len` blocks at `width`: returns the first block
    /// whose sum must be recomputed. A new width makes all of them stale.
    pub(super) fn begin(&mut self, width: u16, len: usize) -> usize {
        if width != self.width {
            self.width = width;
            self.clean = 0;
        }
        let start = self.clean.min(len);
        self.prefix.truncate(start + 1);
        if self.prefix.is_empty() {
            self.prefix.push(0);
        }
        start
    }

    /// Block `index` (laid out from [`Self::begin`] on, in order) takes
    /// `lines` lines.
    pub(super) fn record(&mut self, index: usize, lines: usize) {
        let base = self.prefix.get(index).copied().unwrap_or(0);
        self.prefix.push(base + lines);
    }

    /// Every one of the `len` blocks is summed: returns the total lines.
    pub(super) fn finish(&mut self, len: usize) -> usize {
        self.clean = len;
        self.lines_before(len).unwrap_or(0)
    }

    /// The laid-out lines before block `index`, while its sum is clean
    /// (`None`: it changed since the last layout).
    pub(super) fn start_line(&self, index: usize) -> Option<usize> {
        (index <= self.clean).then(|| self.lines_before(index))?
    }

    /// The summed lines before block `index`, clean or not.
    fn lines_before(&self, index: usize) -> Option<usize> {
        self.prefix.get(index).copied()
    }

    /// How many of `len` blocks have a clean sum, and the lines they take.
    pub(super) fn known(&self, len: usize) -> Known {
        let blocks = self.clean.min(self.prefix.len().saturating_sub(1)).min(len);
        Known {
            blocks,
            lines: self.lines_before(blocks).unwrap_or(0),
        }
    }

    /// The block among the first `known` whose lines hold line `idx`
    /// (which lies before the known blocks' end), and the lines before it.
    pub(super) fn block_holding(&self, idx: usize, known: usize) -> Known {
        let at = self
            .prefix
            .get(..=known)
            .unwrap_or_default()
            .partition_point(|&lines| lines <= idx)
            .saturating_sub(1);
        Known {
            blocks: at,
            lines: self.lines_before(at).unwrap_or(0),
        }
    }

    /// A block was laid out (tests count them).
    #[cfg(test)]
    pub(super) fn note_laid_out(&mut self) {
        self.laid_out += 1;
    }

    /// A block was visited to find or copy lines (tests count them).
    #[cfg(test)]
    pub(super) fn note_walked(&self) {
        self.walked.set(self.walked.get() + 1);
    }
}

/// A count of blocks from the transcript's start and the laid-out lines
/// they take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Known {
    /// Blocks `[0, blocks)`.
    pub(super) blocks: usize,
    /// Their lines.
    pub(super) lines: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three blocks of 2, 3 and 1 lines laid out at `width`.
    fn laid(width: u16) -> LayoutCache {
        let mut cache = LayoutCache::default();
        let start = cache.begin(width, 3);
        assert_eq!(start, 0);
        for (i, lines) in [2, 3, 1].into_iter().enumerate() {
            cache.record(i, lines);
        }
        assert_eq!(cache.finish(3), 6);
        cache
    }

    #[test]
    fn a_layout_sums_the_lines_before_each_block() {
        let cache = laid(80);
        assert_eq!(cache.start_line(0), Some(0));
        assert_eq!(cache.start_line(1), Some(2));
        assert_eq!(cache.start_line(2), Some(5));
        assert_eq!(
            cache.known(3),
            Known {
                blocks: 3,
                lines: 6
            }
        );
        assert_eq!(
            cache.block_holding(4, 3),
            Known {
                blocks: 1,
                lines: 2
            }
        );
    }

    #[test]
    fn an_edit_makes_the_blocks_from_it_on_stale() {
        let mut cache = laid(80);
        cache.touch(1);
        assert_eq!(cache.start_line(1), Some(2), "its own start holds");
        assert_eq!(cache.start_line(2), None);
        assert_eq!(
            cache.known(3),
            Known {
                blocks: 1,
                lines: 2
            }
        );
        assert_eq!(cache.begin(80, 3), 1, "only the changed block on");
    }

    #[test]
    fn a_new_width_or_invalidating_all_makes_every_block_stale() {
        let mut cache = laid(80);
        assert_eq!(cache.begin(60, 3), 0);
        let mut cache = laid(80);
        cache.invalidate_all();
        assert_eq!(cache.start_line(1), None);
        assert_eq!(cache.begin(80, 3), 0);
    }
}
