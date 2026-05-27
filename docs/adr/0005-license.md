# 0005: license

status: proposed

date: 2026-05-27

## context

mycel is intended as a personal local-first agent harness with possible adapter and ecosystem reuse.

the requested license candidate is MIT.

## decision

use the MIT license.

## rationale

MIT is short, permissive, familiar, and friendly to downstream experiments. **confidence: solid.**

MIT avoids forcing license strategy decisions before the substrate model is proven. **confidence: directional.**

## consequences

- commercial and closed-source reuse is allowed. **confidence: solid. load-bearing.**
- patent protection is less explicit than Apache-2.0. **confidence: solid. load-bearing.**
- contributors should understand that permissive reuse is intentional.

## alternatives

| option | result |
| --- | --- |
| Apache-2.0 | stronger patent language, more text. **confidence: solid.** |
| MPL-2.0 | file-level copyleft, more governance weight. **confidence: solid.** |
| AGPL-3.0 | strong network copyleft, poor fit for early adapter experimentation. |

## unresolved

- whether future distributed substrate services need a different license boundary.
