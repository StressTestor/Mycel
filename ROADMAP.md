# roadmap

confidence key: **solid** means verified or strongly supported. **directional** means the shape is likely right, but details may change. **vibes** means a useful hypothesis, not a fact.

## v0.1: fail-pattern immunity

ship one mechanism: **fail-pattern immunity**. **confidence: directional. load-bearing.**

scope:

- define an antibody record: signature, source, severity, confidence, refusal mode, expiry, examples, and remediation. **confidence: directional. load-bearing.**
- ingest failed run records and Sentinel block logs as antibody candidates. **confidence: directional. load-bearing.**
- evaluate proposed agent runs against the registry before launch. **confidence: directional. load-bearing.**
- return one of three outcomes: refuse, warn, allow. **confidence: directional. load-bearing.**
- write human-readable substrate projections after each decision. **confidence: directional.**

argument for this pick:

- it proves Mycel can remember a failure and enforce a future decision. **confidence: directional. load-bearing.**
- it has a clean integration path with Sentinel. **confidence: directional. load-bearing.**
- it is measurable with small fixtures before any autonomous spawning exists. **confidence: directional.**

argument against:

- it does not exercise the full agent lifecycle. **confidence: directional.**
- bad signatures can create false refusals, so v0.1 must include expiry and human override. **confidence: directional. load-bearing.**

decision: start here anyway. **confidence: directional. load-bearing.**

## v0.2: decay-pruned context

add scheduled ttl-tiered context maintenance. **confidence: directional. load-bearing.**

why second:

- it needs the substrate store from v0.1. **confidence: directional.**
- it brings PromptPressure confidence tiers into the core model. **confidence: directional.**
- it creates the first compaction loop without requiring spawning. **confidence: directional.**

minimum:

- solid findings keep long ttl and can be preserved verbatim. **confidence: directional.**
- directional findings get medium ttl and distillation. **confidence: directional.**
- vibes findings decay quickly and never ship as fact. **confidence: directional. load-bearing.**
- no-compost records are preserved by policy. **confidence: directional. load-bearing.**

## v0.3: sclerotia

add dormancy and condition-triggered wake records. **confidence: directional. load-bearing.**

why third:

- blocked work needs durable state before autonomous chains become useful. **confidence: directional.**
- wake conditions reuse the same typed condition evaluator needed for later spawning. **confidence: directional.**

minimum:

- serialize task state, blocker, attempted paths, next command, and wake conditions. **confidence: directional.**
- scan local conditions and mark dormant work as wakeable. **confidence: directional.**
- require user or harness confirmation before live resume in v0.3. **confidence: directional.**

## v0.4: self-spec on death

add final-act handoff specs. **confidence: directional.**

why fourth:

- antibodies and sclerotia make death records safer and less lossy. **confidence: directional.**
- it can start manual: an agent writes the next spec, but the substrate does not auto-spawn it yet. **confidence: directional.**

minimum:

- define next-agent spec schema. **confidence: directional.**
- attach confidence tags, preconditions, refusal reasons, and inherited context. **confidence: directional.**
- dedupe specs by task signature. **confidence: directional.**

## v0.5: mycorrhizal kin-sharing

add targeted context bequests to related tasks. **confidence: directional.**

why fifth:

- kin-sharing needs stable task signatures from antibodies, sclerotia, and self-specs. **confidence: directional.**
- STs-Mission-Control can be evaluated here as a kin-detection layer. **confidence: directional.**

minimum:

- define kin signature and similarity rules. **confidence: directional.**
- bequeath only scoped context, never full transcript broadcast. **confidence: directional. load-bearing.**
- record source, recipient, payload type, and expiry. **confidence: directional.**

## v0.6: spore-based plugin discovery

add typed spore manifests from completed runs. **confidence: directional.**

why sixth:

- spores are more useful after the substrate can decay, dedupe, and route related context. **confidence: directional.**
- OpenClaw manifest lessons can inform the cheap metadata boundary. **confidence: solid for OpenClaw manifest precedent, directional for Mycel design.**

minimum:

- define spore schema for completed work, adjacent work, required conditions, and interop hints. **confidence: directional.**
- catalog spores locally. **confidence: directional.**
- mark germination candidates without auto-spawn. **confidence: directional.**

## v0.7: substrate-conditioned spawning

add condition-matched agent firing. **confidence: directional. load-bearing.**

why seventh:

- this is the riskiest behavior, so it should come after refusal, dormancy, decay, and handoff controls. **confidence: directional. load-bearing.**

minimum:

- match typed environmental tuples against spores and sclerotia. **confidence: directional.**
- run dry-plan mode before launch. **confidence: directional.**
- enforce antibody checks before spawn. **confidence: directional. load-bearing.**

## v0.8: interop hardening

make Mycel skills partially transferable to Hermes and OpenClaw. **confidence: directional.**

minimum:

- export Mycel-native skills to agentskills.io-compatible shape where possible. **confidence: directional.**
- export Hermes-compatible basic skills without ecology metadata. **confidence: directional.**
- export OpenClaw-compatible plugin or skill metadata without claiming substrate features. **confidence: directional.**

## v0.9: evals and diligence

prove the loops with fixtures and behavioral evals. **confidence: directional.**

minimum:

- antibody false-positive and false-negative fixtures. **confidence: directional. load-bearing.**
- decay ttl fixtures by confidence tier. **confidence: directional.**
- sclerotia wake-condition fixtures. **confidence: directional.**
- PromptPressure integration experiments. **confidence: directional.**

## v1.0: local-first ecological harness

v1.0 means the seven core mechanisms work together locally. **confidence: directional. load-bearing.**

exit criteria:

- every spawn path passes antibody evaluation. **confidence: directional. load-bearing.**
- dormant work can wake from typed conditions. **confidence: directional.**
- death records can produce specs, spores, and kin bequests. **confidence: directional.**
- context decay runs on schedule, not only at overflow. **confidence: directional. load-bearing.**
- all substrate projections can be regenerated from the canonical store. **confidence: directional. load-bearing.**

## post-v1

- lifestyle classification: parasite, saprophyte, symbiote. **confidence: vibes.**
- mycoheterotroph detection for freeloader patterns. **confidence: vibes.**
- distribution layer for selected spores, skills, and antibodies. **confidence: vibes.**
