# contributing

mycel is in design-first mode. no production source directories should be added until the initial ADRs are settled. **confidence: solid. load-bearing.**

## basics

- use conventional commits, for example `docs(adr): choose substrate format`.
- keep empirical claims, predictions, and load-bearing assumptions confidence-tagged as **solid**, **directional**, or **vibes**.
- leave decisions and scope items untagged.
- never ship vibes-tier claims as facts.
- prefer local-first designs. cloud services need a written reason.
- update ADRs when a design decision changes.
- keep public prose direct, specific, and low-gloss.

## design changes

for now, design changes should include:

1. the problem.
2. the proposed decision.
3. tradeoffs.
4. confidence tags for claims that need them.
5. affected roadmap or ADR files.

## code changes

code changes are intentionally out of scope until the project exits initialization. **confidence: solid. load-bearing.**
