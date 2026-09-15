# Rich surface v2 design fixtures

These fixtures are normative inputs from rich surfaces. freezes decisions; it
does not advertise or implement protocol `0.3`.

- `compat-wire.tsv` locks the exact existing `0.1` and `0.2` query/reply bytes.
  `prismattyc-protocol/tests/adr0014_fixtures.rs` exercises them against the current
  encoder.
- `capability-v0.3.tsv` locks the proposed next profile's query/reply APC bodies
  and canonical field order. A later implementation must produce those bodies
  byte-for-byte before advertising `0.3`.
- `state-v0.3.tsv` freezes state-machine outcomes independent of a Rust type
  layout. Each row has six tab-separated columns and is checked for uniqueness
  and required negative cases.

Hex fields use lowercase, two digits per byte, with no `0x` prefix or spaces.
APC bytes include the `ESC _` introducer and `ESC \\` terminator.
