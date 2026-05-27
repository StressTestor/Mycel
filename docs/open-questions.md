# open questions

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## load-bearing

- what is the minimum antibody signature that catches repeat failures without blocking adjacent valid work? **confidence: directional. load-bearing.**
- should antibody matching be deterministic only in v0.1, or allow optional embedding similarity behind a local feature flag? **confidence: directional. load-bearing.**
- what Sentinel log fields are stable enough to ingest as first-class antibody source fields? **confidence: directional. load-bearing.**
- should substrate files be edited by humans, or treated as generated projections with a separate override file? **confidence: directional. load-bearing.**
- what is the exact contract between Mycel and STs-Mission-Control for kin detection? **confidence: directional. load-bearing.**
- should sclerotia wake automatically launch agents, or only mark work as wakeable until v1.0? **confidence: directional. load-bearing.**
- how should no-compost records be represented so they cannot be accidentally summarized away? **confidence: directional. load-bearing.**
- what belongs in a Mycel-native skill that cannot be represented in Hermes or OpenClaw exports? **confidence: directional. load-bearing.**

## research

- compare SQLite full-text search, sqlite-vec, and deterministic tag matching for antibody lookup. **confidence: directional.**
- inspect Sentinel block log schema before freezing antibody ingestion. **confidence: directional.**
- inspect PromptPressure output format before freezing confidence-tier imports. **confidence: directional.**
- inspect STs-Mission-Control task identity model before designing kin signatures. **confidence: directional.**
- decide whether canonical markdown files should be regenerated on every substrate mutation or during maintenance only. **confidence: directional.**
- define how to redact secrets from spores, antibodies, and death specs. **confidence: directional. load-bearing.**
- define an eval fixture format before implementation starts. **confidence: directional.**

## deliberately parked

- lifestyle classification names and thresholds. **confidence: vibes.**
- mycoheterotroph detection heuristics. **confidence: vibes.**
- remote distribution, trust, signing, and revocation. **confidence: vibes.**
- hosted catalog or marketplace. **confidence: vibes.**
