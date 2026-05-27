# open questions

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## load-bearing

- what is the minimum antibody signature that catches repeat failures without blocking adjacent valid work?
- should antibody matching be deterministic only in v0.1, or allow optional embedding similarity behind a local feature flag?
- what Sentinel log fields are stable enough to ingest as first-class antibody source fields?
- should substrate files be edited by humans, or treated as generated projections with a separate override file?
- what is the exact contract between Mycel and STs-Mission-Control for kin detection?
- should sclerotia wake automatically launch agents, or only mark work as wakeable until v1.0?
- how should no-compost records be represented so they cannot be accidentally summarized away?
- what belongs in a Mycel-native skill that cannot be represented in Hermes or OpenClaw exports?
- how much interop design belongs in v0.1 before it slows down antibody work?

## research

- compare SQLite full-text search, sqlite-vec, and deterministic tag matching for antibody lookup.
- inspect Sentinel block log schema before freezing antibody ingestion.
- inspect PromptPressure output format before freezing confidence-tier imports.
- inspect STs-Mission-Control task identity model before designing kin signatures.
- decide whether canonical markdown files should be regenerated on every substrate mutation or during maintenance only.
- define how to redact secrets from spores, antibodies, and death specs.
- define an eval fixture format before implementation starts.
- define a tolerable false-positive rate for antibody checks on real projects.
- decide whether the v0.1 interop loss matrix should include agentskills.io strictly or treat it as a best-effort target.

## deliberately parked

- lifestyle classification names and thresholds. **confidence: vibes.**
- mycoheterotroph detection heuristics. **confidence: vibes.**
- remote distribution, trust, signing, and revocation. **confidence: vibes.**
- hosted catalog or marketplace. **confidence: vibes.**
