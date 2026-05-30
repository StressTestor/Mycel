# 8. time injection for deterministic maintenance

## status

accepted

date: 2026-05-30

## context

decay and maintenance operations compare records against a "current time" to decide whether a ttl
has expired. if maintenance code calls `OffsetDateTime::now_utc()` internally, fixtures are
non-deterministic: the same fixture produces a different result depending on when the test runs.

this matters for the v0.2 maintenance engine and every subsequent mechanism that inherits decay
scheduling (v0.3 wake conditions, v0.4 sclerotia ttl).

v0.1 code (antibody evaluation, sentinel ingestion) already accepts `now: DateTime<Utc>` at call
sites. that pattern carries forward into all new maintenance code.

## decision

all NEW maintenance, decay, and import code accepts `now: i64` (unix timestamp) as an explicit
parameter instead of reading the system clock internally. the CLI layer is responsible for passing
`OffsetDateTime::now_utc().unix_timestamp()` when calling maintenance functions. test fixtures pass
a fixed timestamp.

v0.1 code is left unchanged to avoid regression. the convention is inherited, not retrofitted.

v0.3 wake-condition evaluation and v0.4 sclerotia dormancy checks inherit this pattern when
implemented.

## consequences

- maintenance fixtures are fully deterministic regardless of wall clock. **confidence: solid.**
- the CLI boundary is the only place the system clock is read for decay operations.
- time-travel tests (simulate future expiry without advancing the real clock) work without mocking.
- any maintenance function that accepts `now: i64` is testable in isolation. **confidence: solid. load-bearing.**
- adding a dry-run mode or backfill operation does not require clock manipulation.

## unresolved

- whether `now: i64` (unix seconds) should be wrapped in a newtype for type safety in v0.3+.
- whether the CLI should validate that `now` is not absurdly far in the past or future.
