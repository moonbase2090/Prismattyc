//! Screen-local storage for trailing grapheme scalars.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use crate::{Cell, MAX_COMBINING_MARKS};

const COLLECTION_SLACK: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Cluster {
    marks: [char; MAX_COMBINING_MARKS],
    len: u8,
}

impl Cluster {
    fn from_slice(marks: &[char]) -> Self {
        let mut cluster = Self {
            marks: ['\0'; MAX_COMBINING_MARKS],
            len: marks.len() as u8,
        };
        cluster.marks[..marks.len()].copy_from_slice(marks);
        cluster
    }

    fn as_slice(&self) -> &[char] {
        &self.marks[..self.len as usize]
    }
}

/// Index zero denotes an empty tail and needs no allocation. Entries are
/// immutable. Grid moves can therefore copy handles without reference counts.
#[derive(Debug)]
pub(super) struct ClusterStore {
    entries: Vec<Cluster>,
    index: HashMap<Cluster, u32>,
    collect_at: usize,
    retired_rows: usize,
    identity: Arc<()>,
    import_source: Weak<()>,
    import_handles: Vec<u32>,
}

impl Default for ClusterStore {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
            collect_at: COLLECTION_SLACK,
            retired_rows: 0,
            identity: Arc::new(()),
            import_source: Weak::new(),
            import_handles: Vec::new(),
        }
    }
}

impl Clone for ClusterStore {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            index: self.index.clone(),
            collect_at: self.collect_at,
            ..Self::default()
        }
    }
}

impl ClusterStore {
    /// Cache only handles from one immutable-index store at a time. A weak
    /// identity keeps the allocation address unique without retaining cells:
    /// the Weak reserves the Arc control block until the cache releases it.
    /// Cloning and collecting a store both assign a fresh identity.
    /// Switching sources is supported. Each switch drops the translation
    /// cache, so each imported tail needs an intern lookup on its first use.
    pub(super) fn import(&mut self, source: &Self, handle: u32) -> u32 {
        if handle == 0 {
            return 0;
        }
        if self.import_source.as_ptr() != Arc::as_ptr(&source.identity) {
            self.import_source = Arc::downgrade(&source.identity);
            self.import_handles = Vec::new();
        }
        let index = handle as usize - 1;
        if self.import_handles.len() <= index {
            self.import_handles.resize(index + 1, 0);
        }
        let mapped = self.import_handles[index];
        if mapped != 0 {
            return mapped;
        }
        let mapped = self.intern(source.get(handle));
        self.import_handles[index] = mapped;
        mapped
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Payload-capacity estimate. Hash-table control bytes, unused buckets,
    /// identity control blocks, and allocator bookkeeping are excluded.
    pub(super) fn payload_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Cluster>()
            + self.index.capacity() * std::mem::size_of::<(Cluster, u32)>()
            + self.import_handles.capacity() * std::mem::size_of::<u32>()
    }

    pub(super) fn get(&self, handle: u32) -> &[char] {
        if handle == 0 {
            &[]
        } else {
            self.entries[handle as usize - 1].as_slice()
        }
    }

    pub(super) fn intern(&mut self, marks: &[char]) -> u32 {
        if marks.is_empty() {
            return 0;
        }
        let cluster = Cluster::from_slice(marks);
        if let Some(&handle) = self.index.get(&cluster) {
            return handle;
        }
        let handle = u32::try_from(self.entries.len() + 1).expect("cluster handle fits u32");
        self.entries.push(cluster);
        self.index.insert(cluster, handle);
        handle
    }

    pub(super) fn set(&mut self, cell: &mut Cell, marks: &[char]) {
        cell.cluster = self.intern(marks);
        cell.cluster_flags = u8::from(
            marks
                .first()
                .is_some_and(|c| crate::is_regional_indicator(*c)),
        ) | (u8::from(marks.contains(&'\u{fe0f}')) << 1)
            | (u8::from(marks.last() == Some(&crate::ZWJ)) << 2);
    }

    pub(super) fn append(&mut self, cell: &mut Cell, mark: char) -> bool {
        let old = self.get(cell.cluster);
        if cell.wide_cont || old.len() == MAX_COMBINING_MARKS {
            return false;
        }
        let mut marks = ['\0'; MAX_COMBINING_MARKS];
        let len = old.len();
        marks[..len].copy_from_slice(old);
        marks[len] = mark;
        self.set(cell, &marks[..len + 1]);
        true
    }

    pub(super) fn needs_collection(&self) -> bool {
        self.entries.len() >= self.collect_at
    }

    pub(super) fn retire_rows(&mut self, count: usize, live_rows: usize) -> bool {
        // A small cache is bounded by the slack. Reclaim larger stores after
        // enough row replacements, even when new output is entirely ASCII.
        if self.entries.len() <= COLLECTION_SLACK {
            return false;
        }
        self.retired_rows = self.retired_rows.saturating_add(count);
        self.retired_rows >= live_rows.max(COLLECTION_SLACK)
    }

    pub(super) fn finish_collection(&mut self) {
        // Amortize a full live-cell scan across new distinct tails. Repeated
        // text reuses entries and does not trigger collection on each append.
        self.collect_at = self
            .entries
            .len()
            .saturating_mul(2)
            .saturating_add(COLLECTION_SLACK);
    }
}
