# 16. Wake-condition vocabulary

## Status

Accepted.

## Context

A dormant record (ADR 0015) is only useful if "is it time to resume?" can be answered
cheaply, deterministically, and without ambiguity during local maintenance. Free-text wake
conditions ("when the API is ready") fail this: they cannot be evaluated by a program and
they reintroduce the transcript dependency we are trying to remove. The roadmap rollback
trigger is explicit — if wake conditions are too vague to evaluate deterministically, v0.5
pauses for condition-vocabulary work before spores. This ADR fixes a typed vocabulary to
avoid that.

## Decision

`WakeCondition` is a closed, internally-tagged enum. Each variant evaluates against a
caller-supplied `WakeWorld` snapshot — never against a hidden clock or the filesystem (ADR
0008 time-injection), so evaluation is pure and reproducible.

`WakeWorld` carries: `now: i64` (unix seconds), and three `BTreeSet<String>`s —
`existing_paths`, `resolved_signatures`, `raised_signals`.

| variant | shape | `is_met(world)` is true when |
| --- | --- | --- |
| `TimeReached` | `{ at: i64 }` | `world.now >= at` |
| `FileExists` | `{ path: String }` | `world.existing_paths` contains `path` |
| `FileAbsent` | `{ path: String }` | `world.existing_paths` does **not** contain `path` |
| `DependencyResolved` | `{ signature: String }` | `world.resolved_signatures` contains `signature` |
| `SignalRaised` | `{ name: String }` | `world.raised_signals` contains `name` |
| `Manual` | (unit) | **never** — always `false` |

`Manual` models "only a human revisit unblocks this." Returning `false` from `is_met` keeps
automated wakeable-detection from ever firing a Manual record; a human must act on it out of
band. `DependencyResolved` reuses task signatures (ADR 0012), so one dormant record can wake
on another's completion. Multiple conditions on one record combine with AND semantics
(`Sclerotium::is_wakeable`).

## Consequences

Every wake condition is a cheap set/integer check, so a maintenance pass can scan all
dormant records against a single `WakeWorld` in linear time with no I/O. The vocabulary is
closed, so a reviewer can audit exactly what can wake a record. The same condition fields are
available for v0.5 spores to share (per the roadmap parallel-work note). Adding a new
condition kind is an additive enum variant plus one `is_met` arm.

Confidence: **directional. load-bearing.** The vocabulary covers the conditions seen so far;
real dormant work may surface a condition that does not fit (e.g. a numeric threshold), which
would be a new variant rather than a redesign.
