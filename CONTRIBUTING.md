# contributing

Mycel is a terminal-only Rust coding-agent harness. Changes should improve the
CLI or its local substrate without widening the product into a browser, editor,
daemon, or vendor control plane.

## boundaries

- Keep the product local-first and terminal-only.
- Do not add browser, editor, daemon, ACP, telemetry, updater, or vendor
  marketplace surfaces.
- Treat `docs/RUST_PORT_PARITY.md` as the record of the completed Rust boundary
  and its intentional exclusions.
- Preserve Workflow, `/hyphae`, and the substrate command family.
- Treat `ARCHITECTURE.md` and accepted ADRs as the current design record.
- Keep empirical claims distinct from predictions and assumptions.

## changes

- Use conventional commits such as `fix(gate): reject a truncated substrate`.
- Keep commits logically bisectable and stage explicit paths.
- Add or update tests for behavioral changes.
- Update `ARCHITECTURE.md` when structure, dependencies, configuration,
  integration boundaries, or deployment behavior changes.
- Do not commit credentials, local substrate data, generated build output, or
  agent scratch files.

## verification

Run the checks relevant to the change. Before merging a broad change, run all
of them:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash tests/e2e/gate-contract.sh
bash tests/e2e/immunity-loop.sh
```
