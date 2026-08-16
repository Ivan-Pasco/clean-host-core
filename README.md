# clean-host-core

The shared runtime library every concrete Clean host links.

`clean-server`, `clean-worker`, `clean-cli`, `clean-browser`, and `clean-edge`
each own an I/O shape and nothing else. Everything below that shape lives here:
configuration, bridge discovery, Component Model composition, the WASI stack,
instance pooling, the capability manifest, reload, health, and shutdown.

Specification:
`foundation/02 components/hosts/clean-host-core/01-specification.md`.

## Crates

| Crate | Purpose |
|---|---|
| `clean-host-core` | The library. No I/O, no Wasm engine, no HTTP. |
| `clean-host-core-wasmtime` | `WasmRuntime` on Wasmtime with async, epoch interruption, and the pooling allocator. |

## The two rules that shape everything here

**CH-01 — the library never owns I/O.** Nothing in `clean-host-core` opens a
socket, binds a port, or polls a queue. The concrete host calls in; this library
never calls out. The only files it touches are the config, the guest and bridge
`.wasm` files, and the capability manifest.

**No stub imports.** Only real implementations are added to the Wasmtime
`Linker`. A missing bridge function surfaces as a load-time import error, never
as a no-op that fails mysteriously at request time
([Platform 16 §16.14](../foundation/03%20platform/16-host-contract-validation.md)).

## Status

M0. Implemented: config parsing, the `WasmRuntime` seam, the Wasmtime adapter,
instance pooling, bridge discovery and WAC composition, Moment 3 load-time
validation (`COM017`), capability manifest emission, the `clean:host/log` sink
seam, and the shared HCV-06 parity helper.

Composition is exercised end to end by `clean-server`, not by this crate's own
tests: there is no `.wasm` fixture here, so the WAC path and the Wasmtime
instantiate path have no coverage inside this workspace.

Not yet implemented, and rejected loudly rather than silently ignored:

- **Signature identity.** Moment 3 checks interface presence and version
  compatibility; the third leg of HCV-03 needs the resolved type graph and lands
  with the bridge work.
- **`wasi:http`.** `wasmtime-wasi` 47 ships `p3` for cli/clocks/filesystem/
  random/sockets, but `wasmtime-wasi-http` has no `p3` feature yet, so
  `wasi:http@0.3.0` and `wasi:http/middleware@0.3.0` (CLNH-32, CH-08) are not
  available. Hosts that own their HTTP surface natively — clean-server does —
  are unaffected at M0.

## Consuming it

Hosts depend on this by path while both repos are pre-1.0 and developed side by
side. These become git or registry pins at M1:

```toml
[workspace.dependencies]
clean-host-core = { path = "../clean-host-core/crates/clean-host-core" }
clean-host-core-wasmtime = { path = "../clean-host-core/crates/clean-host-core-wasmtime" }
```

There is no `host.wit` in this repo. `clean-host-core` has no world of its own —
each concrete host publishes its own `host.wit` at its repo root (HCV-02).
