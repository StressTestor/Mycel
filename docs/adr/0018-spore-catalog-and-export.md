# 18. Spore catalog and inert interop export

## Status

Accepted.

## Context

Spores (ADR 0017) need somewhere to live and a way to leave Mycel. Two requirements shape
this: a local catalog must not accumulate repeats, and exporting a spore to a foreign runtime
must not pretend that runtime enforces Mycel policy. The existing interop loss matrix
(`docs/interop-loss-matrix.md`) already states the rule: exports declare lost features rather
than implying enforcement elsewhere.

## Decision

**Catalog.** `SporeStore` persists spores as JSON plus indexed `signature` and `kind`
columns, mirroring `SpecStore`/`SclerotiumStore`. `SporeStore::catalog` dedupes the input
**before** storing, so the catalog never holds a repeat. The `spores` table is added to
`FULL_SCHEMA_SQL` additively (idempotent `CREATE TABLE IF NOT EXISTS` + two indexes) with no
schema-version bump; `user_version` stays 4.

**Dedupe key.** `dedupe_spores` keys on `(kind, signature)`, keeping the first occurrence
(stable). Keying on the pair — not the signature alone — keeps a completed-work spore and an
adjacent-work spore for the same task distinct, because they describe different discoveries.

**Inert export.** `export_spore` projects a spore into one of the four loss-matrix shapes —
`MycelNative`, `Hermes`, `OpenClaw`, `AgentSkills`. `MycelNative` is lossless (carries the
full manifest, `dropped` empty, `lossless: true`). The three foreign shapes carry a degrading
subset and populate a `dropped` list naming the ecology fields (kind, origin, confidence,
signature) that do **not** survive. No foreign shape carries `confidence` as a live field,
because a live confidence would imply the foreign runtime acts on it. The dropped fields
remain recoverable only from the Mycel substrate, never from the export.

## Consequences

The catalog is dedup-on-write, so repeated discoveries collapse to one record with a stable
identity. Export is honest: a buyer or an interop adapter reading a Hermes/OpenClaw/agentskills
export sees exactly what degraded, and the spore is inert metadata everywhere but Mycel.
Adding a new interop target is a new `InteropShape` variant plus one `export_spore` arm.

Confidence: **directional. load-bearing.** The four shapes match the current loss matrix;
real adapters may need a fifth, which is an additive variant rather than a redesign.
