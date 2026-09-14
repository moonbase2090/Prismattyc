//! Retained-row allocation accounting. Shared cluster storage is separate.

use std::collections::VecDeque;
use std::io::Write;
use std::mem::size_of;

use crate::{Cell, Screen};

pub(super) fn row_limit(columns: usize, rows: usize, bytes: usize) -> usize {
    let row_bytes = columns
        .saturating_mul(size_of::<Cell>())
        .saturating_add(size_of::<Vec<Cell>>() + size_of::<bool>());
    rows.min(bytes / row_bytes)
}

impl Screen {
    /// Maximum bytes retained in scrollback row buffers and deque allocations.
    pub const fn scrollback_byte_budget(&self) -> usize {
        self.max_scrollback_bytes
    }

    /// Payload capacity of the shared cluster table and import cache, in bytes.
    /// This estimate excludes hash-table control bytes, unused buckets,
    /// identity control blocks, and allocator bookkeeping. It is separate from
    /// the retained-row budget because live cells and history share entries.
    pub fn cluster_storage_payload_bytes(&self) -> usize {
        self.clusters.payload_bytes()
    }

    /// Allocated bytes for retained row cells and both history deques.
    ///
    /// History cell vectors have capacity equal to the screen width. Count
    /// spare deque slots too. This excludes live grids, shared cluster storage,
    /// the hyperlink table, and allocator bookkeeping.
    pub fn scrollback_bytes(&self) -> usize {
        // Sample both ends without scanning history on every enqueue.
        debug_assert!(self
            .scrollback
            .front()
            .into_iter()
            .chain(self.scrollback.back())
            .all(|row| row.len() == self.columns && row.capacity() == self.columns));
        self.scrollback
            .len()
            .saturating_mul(self.columns)
            .saturating_mul(size_of::<Cell>())
            .saturating_add(
                self.scrollback
                    .capacity()
                    .saturating_mul(size_of::<Vec<Cell>>()),
            )
            .saturating_add(self.scrollback_wrapped.capacity())
    }

    /// Set the retained-row budget and evict complete oldest rows immediately.
    /// Zero disables history retention. The row-count limit still applies.
    pub fn set_scrollback_byte_budget(&mut self, bytes: usize) {
        self.max_scrollback_bytes = bytes;
        let evicted = self.configure_scrollback(self.columns);
        self.enforce_scrollback_budget();
        if evicted {
            self.collect_clusters();
            self.bump_epoch();
        }
    }

    pub(super) fn note_scrollback_budget(&mut self, columns: usize) {
        if self.scrollback_budget_warned {
            return;
        }
        self.scrollback_budget_warned = true;
        // A failed diagnostic must not interrupt terminal output.
        let _ = writeln!(
            std::io::stderr().lock(),
            "pmux: scrollback byte budget {} reached at {} columns; evicting oldest rows before the {}-row limit",
            self.max_scrollback_bytes, columns, self.max_scrollback
        );
    }

    fn pop_scrollback(&mut self) {
        self.scrollback.pop_front();
        self.scrollback_wrapped.pop_front();
    }

    /// Reconfigure metadata capacity before resizing row buffers or importing.
    /// Drop rows that cannot fit before allocating their new-width buffers.
    pub(super) fn configure_scrollback(&mut self, columns: usize) -> bool {
        let limit = row_limit(columns, self.max_scrollback, self.max_scrollback_bytes);
        let evicted = self.scrollback.len() > limit;
        if evicted && limit < self.max_scrollback {
            self.note_scrollback_budget(columns);
        }
        while self.scrollback.len() > limit {
            self.pop_scrollback();
        }
        let mut rows = VecDeque::with_capacity(limit);
        rows.extend(std::mem::take(&mut self.scrollback));
        self.scrollback = rows;
        let mut wrapped = VecDeque::with_capacity(limit);
        wrapped.extend(std::mem::take(&mut self.scrollback_wrapped));
        self.scrollback_wrapped = wrapped;
        evicted
    }

    pub(super) fn enforce_scrollback_budget(&mut self) {
        // Account actual deque capacities even if an allocator reserves more
        // slots than requested. The empty case must also release spare slots.
        while self.scrollback.len() > self.max_scrollback
            || self.scrollback_bytes() > self.max_scrollback_bytes
        {
            if self.scrollback.is_empty() {
                self.scrollback = VecDeque::new();
                self.scrollback_wrapped = VecDeque::new();
                break;
            }
            if self.scrollback.len() <= self.max_scrollback {
                self.note_scrollback_budget(self.columns);
            }
            self.pop_scrollback();
        }
    }

    pub(super) fn push_scrollback(&mut self, row: Vec<Cell>, wrapped: bool) {
        let limit = row_limit(self.columns, self.max_scrollback, self.max_scrollback_bytes);
        if self.scrollback.len() >= limit && limit < self.max_scrollback {
            self.note_scrollback_budget(self.columns);
        }
        if limit == 0 {
            return;
        }
        // Reserve once after clone or ED3. Pre-eviction avoids capacity growth
        // on every enqueue when the history is full.
        self.scrollback
            .reserve_exact(limit.saturating_sub(self.scrollback.len()));
        self.scrollback_wrapped
            .reserve_exact(limit.saturating_sub(self.scrollback_wrapped.len()));
        while self.scrollback.len() >= limit {
            self.pop_scrollback();
        }
        self.scrollback.push_back(row);
        self.scrollback_wrapped.push_back(wrapped);
        self.enforce_scrollback_budget();
    }
}
