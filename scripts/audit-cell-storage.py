#!/usr/bin/env python3
"""Add counters to a disposable core checkout for the PT-263 workload audit.

Never use an instrumented library for throughput measurements. Counter order:
rows moved, cells moved, history cells copied, cell assignments, cluster sets,
intern hits, intern misses, import hits, import misses. The measured workload
uses narrow ASCII bases and U+0301 only. This is not a general terminal profiler.
"""
from pathlib import Path
import sys

root = Path(sys.argv[1]) / 'crates/prismattyc-core/src'
p = root / 'lib.rs'
s = p.read_text()
assert 'pt263_audit_reset' not in s
s += '''
static PT263_COUNTS: [std::sync::atomic::AtomicU64; 9] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 9];
fn pt263_count(index: usize, count: usize) {
    PT263_COUNTS[index].fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
}
pub fn pt263_audit_reset() {
    for count in &PT263_COUNTS { count.store(0, std::sync::atomic::Ordering::Relaxed); }
}
pub fn pt263_audit_take() -> [u64; 9] {
    std::array::from_fn(|i| PT263_COUNTS[i].load(std::sync::atomic::Ordering::Relaxed))
}
'''

def inject(source, anchor, code, expected=1):
    assert source.count(anchor) == expected, (anchor, source.count(anchor))
    return source.replace(anchor, code + '\n' + anchor)

a = s.index('    fn apply_scroll_event(')
b = s.index('    fn mark_cursor_cells(', a)
part = inject(s[a:b], 'buf.cells.copy_within(src_start..src_end, dst);',
           'pt263_count(0, (src_end-src_start)/columns); pt263_count(1, src_end-src_start);', 2)
s = s[:a] + part + s[b:]
s = inject(s, 'buf.cells.copy_within(start + columns..end, start);',
           'pt263_count(0, (end-start-columns)/columns); pt263_count(1, end-start-columns);')
s = inject(s, 'let removed: Vec<Cell> = buf.cells[start..start + columns].to_vec();',
           'pt263_count(2, columns);')
s = inject(s, 'buf.cells[blank_start..blank_start + columns].fill(blank);',
           'pt263_count(3, columns);')
s = inject(s, 'buf.cells[index] = cell;', 'pt263_count(3, 1);')
s = inject(s, 'buf.cells[index] = Cell::glyph(character, style).with_hyperlink(hyperlink);',
           'pt263_count(3, 1);')
s = inject(s, 'buf.cells[index] = Cell::default();', 'pt263_count(3, 1);', 2)
p.write_text(s)
p = root / 'clusters.rs'
s = p.read_text()
s = inject(s, 'let mapped = self.intern(source.get(handle));', 'crate::pt263_count(8, 1);')
s = inject(s, 'return mapped;', 'crate::pt263_count(7, 1);')
s = inject(s, 'return handle;', 'crate::pt263_count(5, 1);')
s = inject(s, 'self.entries.push(cluster);', 'crate::pt263_count(6, 1);')
s = inject(s, 'cell.cluster = self.intern(marks);', 'crate::pt263_count(4, 1);')
p.write_text(s)
