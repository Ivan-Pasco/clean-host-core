> Moved from `clean-language-foundation` — `02 components/hosts/clean-host-core/implementation-notes.md`, at foundation commit c72d093 (2026-09-11). Informative: the law it refers to lives in foundation, cited by id; nothing here binds another implementation (SDD-02).

# Clean-Host-Core — Reference Implementation Notes

This file records the reference wasmtime configuration that backs guest memory in the reference hosts. None of it is normative: the observable contract lives in the spec chapters — memory layout and constants, tier limits and the trap on exceeding them (05 §5.3), stable addresses, memory64 acceptance, and interruption behavior (03 — Memory Model). An embedder that produces equivalent guest-observable behavior by other means (browser hosts have no wasmtime at all) is conformant.

---

## Why this file exists

Engine configuration is mechanism, not spec (SDD-02), but it is worth recording: the memory contracts were designed against these exact wasmtime capabilities. Content relocated from the retired ADR-0007 (Accepted 2026-08-01) on 2026-08-22, mirroring clean-server's notes and the compiler's notes.

## 1. Reference wasmtime configuration

- **Reservation and guards (64-bit hosts):** `memory_reservation(4 GiB)` virtual per instance, `memory_guard_size(32 MiB)`, `signals_based_traps(true)`, and `memory_may_move(false)`. The full reservation plus guard lets wasmtime elide explicit bounds checks (a 1.2×–1.8× win on hot paths); static addresses uphold the bump allocator's stable-address invariant.
- **Pooling allocator:** `PoolingAllocationConfig` with `max_memory_size` set to the tier limit, `total_memories` sized to the worker pool, and `linear_memory_keep_resident(2 MiB)` to amortize instantiation across requests.
- **StoreLimits:** every guest store gets `StoreLimitsBuilder` with `memory_size(tier.max_bytes())`, `trap_on_grow_failure(true)`, `table_elements(tier.max_table_elements())`, and `instances(1)`. `trap_on_grow_failure` is how the reference stack delivers the observable contract that a failed grow traps at the offending call site instead of leaking `-1` into guest arithmetic.
- **Interruption — epoch, everywhere:** request-scoped guests are bound to an epoch counter advanced on a timer (server default 5 s, canvas 100 ms; operator surface: `epoch-interval` in `host.toml`). Epoch interruption is chosen because its steady-state overhead is negligible. The compile-time sandbox also uses epoch interruption (see the compiler's notes §3); no fuel metering is used anywhere in the reference stack.
- **Core dump on trap:** enabled in the reference engine so host trap reports can carry the capture that 06 — Error Reporting describes.

## 2. Composition tooling

- **[WAC](https://github.com/bytecodealliance/wac)** — the composer behind CLNH-25 and 18 §4.4 COMP-20..COMP-24. The reference host-core links **`wac-graph` pinned at `0.10`** (the shipped `Cargo.toml` value, ratified by evidence 2026-08-22 — an earlier draft proposed `^0.7` anchored to `wasm-tools ^0.220`; both were stale against the shipped tree). The **library API** is used, never the `wac` CLI, so process-boundary and lockfile semantics stay clean; `wac-parser` is **not** linked — `compose.rs` uses only the graph API. The pin is recorded in the build manifest (14 §14.8) and lockfile-verified like every other tool (Platform 00 §9 Foundational stack, choice 8); a SemVer-major bump is an owner-gated change. Non-normative as always: any Component Model composer satisfying COMP-20..COMP-24 is conformant. The compiler links no WAC — composition is this component's job.

## 3. What this file does NOT cover

The guest-visible memory contract (layout, allocator behavior, representations, traps) is owned by 03 — Memory Model and 05 — Memory Policy. The host-core component contract (instantiation, composition, capability wiring) is owned by 01-specification.md.

---

## Metadata

- **Status:** Informative (implementation record; never normative)
- **Audience:** Anyone building, reviewing, or replacing a reference host embedder
- **References:** 03 — Memory Model, 05 — Memory Policy, SDD-02
