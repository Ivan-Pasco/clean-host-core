//! `clean-host-core` — the shared runtime library every concrete Clean host links.
//!
//! Concrete hosts (`clean-server`, `clean-worker`, `clean-cli`, `clean-browser`,
//! `clean-edge`) own an I/O shape and nothing else. Everything below that shape —
//! config, bridge discovery, Component Model composition, the WASI stack,
//! instance pooling, the capability manifest, reload, health, shutdown — lives
//! here so all hosts get the same guarantees.
//!
//! Spec: `foundation/02 components/hosts/clean-host-core/01-specification.md`.
//!
//! ## CH-01 — this library never owns I/O
//!
//! Nothing here opens a socket, binds a port, or polls a queue. The concrete
//! host calls in; this library never calls out. The only files it touches are
//! the config, the guest/bridge `.wasm` files, and the capability manifest.

pub mod bridge;
pub mod compose;
pub mod config;
pub mod envelope;
pub mod log;
pub mod manifest;
pub mod parity;
pub mod pool;
pub mod runtime;
pub mod validate;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

pub use bridge::DiscoveredBridge;
pub use compose::UnsatisfiedImport;
pub use config::{DeploymentMode, GuestBlock, HostBlock, HostConfig, RuntimeBlock};
pub use envelope::{EnvelopeError, EnvelopeImpl, EnvelopeRegistry};
pub use log::{Level, LogSink, Logger, Record, StderrSink};
pub use manifest::{CapabilitiesManifest, CapabilityEntry, CapabilityStatus};
pub use pool::{InstanceGuard, InstancePool, PoolConfig, PoolError, PoolHealth};
pub use runtime::{Instance, LoadedComponent, RuntimeCapabilities, RuntimeError, WasmRuntime};
pub use validate::{check_compliance, ComplianceReport, InterfaceRef, Violation};

/// Every way host setup or operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("bridge discovery failed: {0}")]
    BridgeDiscovery(String),
    #[error("composition failed: {0}")]
    Composition(String),
    /// A Platform 16 check refused the guest. Carries the rendered `COM017`.
    #[error("{0}")]
    Validation(String),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("envelope error: {0}")]
    Envelope(String),
    #[error("shutdown error: {0}")]
    Shutdown(String),
}

impl From<RuntimeError> for HostError {
    fn from(e: RuntimeError) -> Self {
        HostError::Runtime(e.to_string())
    }
}

impl From<EnvelopeError> for HostError {
    fn from(e: EnvelopeError) -> Self {
        HostError::Envelope(e.to_string())
    }
}

/// Health snapshot the concrete host surfaces however it likes (§11.1).
#[derive(Debug, Clone)]
pub struct HealthReport {
    pub composed: bool,
    pub bridges: Vec<BridgeHealth>,
    pub pool: Option<PoolHealth>,
    pub uptime: Duration,
    pub last_reload_at: Option<SystemTime>,
    pub last_reload_success: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct BridgeHealth {
    pub interface: String,
    pub component: String,
    pub version: String,
}

/// What the concrete host provides directly, so composition and the manifest
/// know which imports are already satisfied without a bridge.
#[derive(Debug, Clone, Default)]
pub struct HostProvided {
    /// Interfaces the host implements natively — for clean-server, the
    /// `clean:host/*` surface from server §1.3.1.
    pub interfaces: Vec<String>,
}

struct Composed {
    component: Arc<dyn LoadedComponent>,
    pool: Arc<InstancePool>,
}

/// The main entry point a concrete host constructs at startup.
pub struct Host {
    config: HostConfig,
    runtime: Box<dyn WasmRuntime>,
    envelopes: EnvelopeRegistry,
    provided: HostProvided,
    composed: RwLock<Option<Composed>>,
    started: Instant,
    generation: Arc<AtomicU64>,
    reload_state: Mutex<(Option<SystemTime>, Option<bool>)>,
    warnings: Mutex<Vec<String>>,
    /// Where `clean:host/log` records go (CLNH-66).
    logger: RwLock<Arc<Logger>>,
}

impl Host {
    /// Build a host from a validated config.
    pub fn new(config: HostConfig, runtime: Box<dyn WasmRuntime>) -> Result<Self, HostError> {
        Ok(Self {
            config,
            runtime,
            envelopes: EnvelopeRegistry::new(),
            provided: HostProvided::default(),
            composed: RwLock::new(None),
            started: Instant::now(),
            generation: Arc::new(AtomicU64::new(0)),
            reload_state: Mutex::new((None, None)),
            warnings: Mutex::new(Vec::new()),
            logger: RwLock::new(Arc::new(Logger::default())),
        })
    }

    /// Declare the interfaces the concrete host implements natively.
    ///
    /// These satisfy guest imports without a bridge, and appear as `active` in
    /// the capability manifest.
    pub fn set_host_provided(&mut self, provided: HostProvided) {
        self.provided = provided;
    }

    /// Register a host-side envelope implementation (§8). Must happen before
    /// [`Host::compose`].
    pub fn register_envelope(
        &mut self,
        interface: &str,
        implementation: Box<dyn EnvelopeImpl>,
    ) -> Result<(), HostError> {
        if self.composed.read().unwrap().is_some() {
            return Err(HostError::Envelope(
                "register_envelope must be called before compose".into(),
            ));
        }
        self.envelopes.register(interface, implementation)?;
        Ok(())
    }

    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// Replace the log sink (CLNH-66).
    ///
    /// The default writes to stderr; `clean-server` swaps in one that routes
    /// records through `tracing`, so guest output interleaves with the host's
    /// own request logs instead of racing them on the same fd.
    pub fn set_log_sink(&self, sink: Arc<dyn LogSink>) {
        let level = self.config.host.log_level.as_str();
        let min = log::Level::parse(level).unwrap_or(log::Level::Info);
        *self.logger.write().unwrap() = Arc::new(Logger::new(sink, min));
    }

    /// The current logger, for the concrete host to hand to composed
    /// components.
    pub fn logger(&self) -> Arc<Logger> {
        Arc::clone(&self.logger.read().unwrap())
    }

    /// Non-fatal problems found during composition, for the concrete host to
    /// log. The library does not write to stderr itself (CH-01).
    pub fn warnings(&self) -> Vec<String> {
        self.warnings.lock().unwrap().clone()
    }

    fn warn(&self, message: String) {
        self.warnings.lock().unwrap().push(message);
    }

    pub fn runtime_capabilities(&self) -> RuntimeCapabilities {
        self.runtime.capabilities()
    }

    /// Discover bridges, verify them, compose them with the guest, and fill the
    /// instance pool.
    ///
    /// CH-02: composition happens once, at startup — never per request.
    /// CH-05: any failure here is a startup error with no degraded fallback.
    pub fn compose(&self) -> Result<(), HostError> {
        let composed = self.build_composition()?;
        *self.composed.write().unwrap() = Some(composed);
        Ok(())
    }

    fn build_composition(&self) -> Result<Composed, HostError> {
        let guest_path = &self.config.guest.wasm;

        // §8 open question #7: an unreachable guest is a startup error naming
        // the config key and the resolved path. No retry, no fallback (CH-05).
        if !guest_path.exists() {
            return Err(HostError::Config(format!(
                "guest component not found\n  [guest] wasm = \"{}\"\n  resolved to: {}\n  \
                 (path is resolved relative to {})",
                self.config
                    .guest
                    .wasm
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                guest_path.display(),
                self.config.source_path.display(),
            )));
        }

        let bytes = std::fs::read(guest_path).map_err(|e| {
            HostError::Config(format!(
                "cannot read guest component `{}`: {e}",
                guest_path.display()
            ))
        })?;

        // Inspect the guest before composing, so capability checks run against
        // what it actually imports rather than what it was expected to.
        let guest_probe = self.runtime.load(&bytes).map_err(|e| match e {
            RuntimeError::Load(msg) => HostError::Config(format!(
                "guest component `{}` could not be loaded: {msg}",
                guest_path.display()
            )),
            other => HostError::Runtime(other.to_string()),
        })?;
        let guest_imports = guest_probe.imports();
        let guest_exports = guest_probe.exports();
        drop(guest_probe);

        // §5.1–5.2: discover and validate every configured bridge.
        let introspect = self.runtime.introspector();
        let bridges = bridge::discover(&self.config.bridges, introspect.as_ref())?;
        for b in &bridges {
            bridge::check_imports(b)?;
            let required = InterfaceRef::parse(&b.interface);
            bridge::check_version(b, &required)?;
        }

        // SRVH-01/02: every imported capability must be provided, and the
        // diagnostic names the `[bridges]` key that is missing.
        let mut provided = self.provided.interfaces.clone();
        provided.extend(self.envelopes.interfaces());
        compose::check_capabilities(&guest_imports, &provided, &bridges)?;

        for unused in compose::unused_bridges(&guest_imports, &bridges) {
            // CH-04: not granted, and said out loud — a configured-but-unused
            // bridge is usually a stale config line.
            self.warn(format!(
                "`[bridges]` configures `{unused}`, which the guest does not import; not composed"
            ));
        }

        // Only compose bridges the guest actually asked for.
        let wanted: Vec<bridge::DiscoveredBridge> = bridges
            .into_iter()
            .filter(|b| {
                let path = InterfaceRef::parse(&b.interface).path;
                guest_imports
                    .iter()
                    .any(|i| InterfaceRef::parse(i).path == path)
            })
            .collect();

        let composed_bytes =
            compose::compose(&bytes, &self.config.guest.name, &guest_exports, &wanted)?;

        let component = self.runtime.load(&composed_bytes).map_err(|e| {
            HostError::Composition(format!("composed component could not be loaded: {e}"))
        })?;
        let component: Arc<dyn LoadedComponent> = Arc::from(component);

        // Moment 3 (HCV-01): refuse a non-compliant guest with COM017 rather
        // than letting it trap at first call.
        self.validate_guest(component.as_ref())?;

        let pool = InstancePool::new(
            Arc::clone(&component),
            PoolConfig {
                instances_min: self.config.runtime.instances_min,
                instances_max: self.config.runtime.instances_max,
                instance_idle: self.config.runtime.instance_idle,
                // The concrete host decides how long a checkout may wait; the
                // server surfaces exhaustion as 503 per §1.4.2.
                checkout_timeout: Duration::from_secs(5),
            },
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        )
        .map_err(HostError::from)?;

        Ok(Composed {
            component,
            pool: Arc::new(pool),
        })
    }

    fn validate_guest(&self, component: &dyn LoadedComponent) -> Result<(), HostError> {
        let imports: Vec<String> = component
            .imports()
            .into_iter()
            // Ambient WASI and clean:host/* are always provided (CH-03), so
            // they are not part of the compliance comparison.
            .filter(|i| !validate::is_ambient_interface(&InterfaceRef::parse(i).path))
            .collect();

        let mut available = self.provided.interfaces.clone();
        available.extend(self.envelopes.interfaces());
        available.extend(self.config.bridges.keys().cloned());

        let report = check_compliance(&imports, &available);
        if !report.is_compliant() {
            return Err(HostError::Validation(report.com017()));
        }
        Ok(())
    }

    /// Write `host-capabilities.cln` (§9). Must be called after `compose`.
    pub fn emit_manifest(&self) -> Result<(), HostError> {
        let guard = self.composed.read().unwrap();
        let composed = guard
            .as_ref()
            .ok_or_else(|| HostError::Composition("emit_manifest called before compose".into()))?;

        let _ = &composed.component;
        let mut entries = Vec::new();

        // Host-provided interfaces are live whether or not this particular
        // guest imports them: the manifest describes the deployment, not the
        // guest's usage of it.
        for iface in &self.provided.interfaces {
            entries.push(CapabilityEntry {
                interface: iface.clone(),
                status: CapabilityStatus::Active,
                component: Some(format!("{} (host-provided)", self.config.host.name)),
                reason: None,
            });
        }

        for (iface, path) in &self.config.bridges {
            entries.push(CapabilityEntry {
                interface: iface.clone(),
                status: CapabilityStatus::Active,
                component: Some(path.display().to_string()),
                reason: None,
            });
        }

        CapabilitiesManifest::new(&self.config, entries).write(self.config.manifest_path())
    }

    /// Check out one instance for a single invocation.
    ///
    /// The returned guard is `'static` — it holds its own reference to the pool
    /// — so the concrete host can move it into an async request task.
    pub fn checkout(&self) -> Result<InstanceGuard, HostError> {
        let guard = self.composed.read().unwrap();
        let composed = guard
            .as_ref()
            .ok_or_else(|| HostError::Composition("checkout called before compose".into()))?;
        let pool = Arc::clone(&composed.pool);
        drop(guard);

        pool.checkout().map_err(|e| match e {
            PoolError::Exhausted { waited } => {
                HostError::Runtime(format!("pool exhausted after {waited:?}"))
            }
            PoolError::Closed => HostError::Shutdown("host is shutting down".into()),
            PoolError::Runtime(e) => HostError::Runtime(e.to_string()),
        })
    }

    /// Re-compose from the current config and atomically swap (§10).
    ///
    /// CLNH-53: on failure the previous composition stays active and the host
    /// does not enter a broken state.
    pub fn reload(&self) -> Result<(), HostError> {
        let result = self.build_composition();
        let mut state = self.reload_state.lock().unwrap();
        *state = (Some(SystemTime::now()), Some(result.is_ok()));

        match result {
            Ok(next) => {
                let previous = self.composed.write().unwrap().replace(next);
                // CLNH-52: the old pool stops handing out instances; guards
                // still outstanding drop their instances when they return.
                if let Some(prev) = previous {
                    prev.pool.close();
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub fn health(&self) -> HealthReport {
        let guard = self.composed.read().unwrap();
        let (composed, pool) = match guard.as_ref() {
            Some(c) => (true, Some(c.pool.health())),
            None => (false, None),
        };
        let (last_reload_at, last_reload_success) = *self.reload_state.lock().unwrap();

        HealthReport {
            composed,
            bridges: self
                .config
                .bridges
                .iter()
                .map(|(iface, path)| BridgeHealth {
                    interface: iface.clone(),
                    component: path.display().to_string(),
                    version: String::new(),
                })
                .collect(),
            pool,
            uptime: self.started.elapsed(),
            last_reload_at,
            last_reload_success,
        }
    }

    /// Graceful shutdown (§11.2): stop accepting checkouts, drain in-flight
    /// work up to `drain`, then drop everything.
    pub fn shutdown(self, drain: Duration) -> Result<(), HostError> {
        let composed = self.composed.write().unwrap().take();
        let Some(composed) = composed else {
            return Ok(());
        };

        composed.pool.close();

        let deadline = Instant::now() + drain;
        while composed.pool.active() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }

        let stranded = composed.pool.active();
        drop(composed);

        if stranded > 0 {
            // CLNH-59: forcibly reclaimed; in-flight work on those instances is
            // lost. Reported so the operator sees it rather than guessing.
            return Err(HostError::Shutdown(format!(
                "{stranded} invocation(s) still in flight after {drain:?}; instances reclaimed"
            )));
        }
        Ok(())
    }
}
