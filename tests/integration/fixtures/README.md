# Integration-test fixtures

This directory is intentionally close to empty. Every fixture an
integration test needs — sample MP4 bytes, the test CA, server certs,
station certs, QR PNGs — is **generated at test setup time** by helpers
in `tests/integration/support/fixtures.rs`.

That keeps the repo small, makes the shape of each test obvious, and
avoids re-generating PEM blobs by hand when crypto choices change (e.g.,
algorithm bumps from rcgen, validity-window adjustments).

If a future test needs a *binary* fixture that can't be generated cheaply
on each run (e.g., a real recorded MP4 from a perchpub interop session),
drop it here and reference it from the test. Document the regeneration
recipe inline.
