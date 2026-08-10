//! The pluggable Wasm runtime seam (CH-06 / CLNH-07).
//!
//! `clean-host-core` never names a Wasm engine. Concrete hosts pick an adapter:
//! `clean-host-core-wasmtime` for server/worker/CLI, a jco adapter for browser.
//!
//! ## Divergence from the spec sketch
//!
//! The Rust surface in clean-host-core §3 sketches `call(&self, instance, export,
//! args: &[Value]) -> Result<Value, _>` — a synchronous, dynamically-typed
//! signature. This module deliberately does not reproduce that shape:
//!
//! - CH-07 (CLNH-08) requires async to be native and always available. A sync
//!   `call` cannot express it, and the spec itself says the invocation signature
//!   is "implementation-defined by the concrete Rust surface".
//! - A dynamically-typed `Value` bag would force every host to re-derive the
//!   component's type information at runtime. Concrete hosts already know the
//!   world they implement, so they invoke typed exports through the adapter
//!   directly.
//!
//! What stays load-bearing here is the *seam*: instantiate, reset, and the
//! capability reports the pool depends on.

use std::fmt;

/// A runtime-level failure. Carried into [`crate::HostError::Runtime`] when it
/// reaches the library's public surface.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("failed to load component: {0}")]
    Load(String),
    #[error("failed to instantiate component: {0}")]
    Instantiate(String),
    #[error("failed to reset instance: {0}")]
    Reset(String),
    #[error("guest trapped: {0}")]
    Trap(String),
    #[error("{0}")]
    Other(String),
}

/// What a runtime adapter can do. Reported once at construction so the pool and
/// the concrete host can adapt rather than probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeCapabilities {
    /// Epoch-based interruption, required for per-request timeouts
    /// (clean-server §1.4.3).
    pub epoch_interruption: bool,
    /// A pooling allocator, required to hit the sub-millisecond checkout
    /// target (clean-server §1.8).
    pub pooling: bool,
}

/// The abstract Wasm runtime seam.
///
/// Implementors are `Send + Sync` because the pool is shared across the
/// concrete host's worker threads.
pub trait WasmRuntime: Send + Sync + 'static {
    /// Load a composed component from bytes, producing a reusable handle.
    ///
    /// This is the expensive step (parse, validate, compile). It happens once
    /// per composition, never per request.
    fn load(&self, composed: &[u8]) -> Result<Box<dyn LoadedComponent>, RuntimeError>;

    fn capabilities(&self) -> RuntimeCapabilities;

    /// Human-readable engine identity, recorded in the capability manifest.
    fn engine_id(&self) -> String;
}

/// A component that has been compiled and is ready to instantiate.
pub trait LoadedComponent: Send + Sync {
    /// Create one fresh instance. The pool calls this to fill itself.
    fn instantiate(&self) -> Result<Box<dyn Instance>, RuntimeError>;

    /// The interfaces this component imports, as `package:ns/iface@version`
    /// strings. Drives the Moment 3 load-time check (HCV-01).
    fn imports(&self) -> Vec<String>;

    /// The interfaces this component exports.
    fn exports(&self) -> Vec<String>;
}

/// One live instance, checked out for exactly one invocation at a time.
pub trait Instance: Send {
    /// Return the instance to a post-instantiation state (CLNH-29).
    ///
    /// Adapters backed by a pooling allocator implement this as a constant-time
    /// linear-memory revert; others may rebuild the instance.
    fn reset(&mut self) -> Result<(), RuntimeError>;

    /// Escape hatch so a concrete host can downcast to its adapter's concrete
    /// instance type and invoke typed exports. The library itself never calls
    /// guest exports (CLNH-70) — it only hands out instances.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl fmt::Debug for dyn LoadedComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedComponent")
            .field("imports", &self.imports().len())
            .field("exports", &self.exports().len())
            .finish()
    }
}
