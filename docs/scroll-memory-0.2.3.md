# Scrollback allocation reuse

Version 0.2.3 avoids allocating a discarded row on each line feed. A row is
copied only when the scroll starts at the top margin and history retention
is enabled for the active screen. Alternate-screen history remains opt-in.

When history reaches its row or byte limit, the oldest retained row supplies
the next row buffer. This reuses its allocation and preserves the existing
history limits. Clearing history still releases those buffers.

## Allocation evidence

The isolated probe warms an 80-by-24 screen with 10,032 line feeds. It then
counts allocations over 1,000 more line feeds. It checks primary and alternate
screens with row limits of zero and 10,000. Alternate history is disabled.

The 0.2.1 implementation made 1,000 allocations totaling 2,880,000 requested
bytes in each case. The candidate makes zero in each case after warm-up.
This measures allocation churn. It does not measure a reduction in retained
RSS, elapsed-time improvement, or display latency.

```bash
cargo run --release --locked -p prismattyc-core --example scroll_allocations
```

The regression test checks that row-limited and byte-limited histories reuse
the evicted buffer, retain the newest text, and remain within the byte budget.
Core and emulator release tests pass. Native throughput, combined coverage,
and mutation validation remain pending.
