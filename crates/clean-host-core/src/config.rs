//! `host.toml` parsing — the `[host]`, `[guest]`, `[runtime]`, and `[bridges]`
//! blocks that CLNH-11 reserves to this library.
//!
//! Schema source of truth:
//! `foundation/02 components/hosts/clean-host-core/schema/host.toml.md`.
//!
//! Concrete hosts own their own block (`[server]`, `[worker]`, ...) and parse it
//! themselves; per CLNH-14 an unknown top-level block is ignored here rather
//! than rejected, which is what lets `[server]` coexist without this library
//! knowing anything about HTTP.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::HostError;

/// A parsed `host.toml`.
///
/// Paths in `[guest] wasm` and `[bridges]` are resolved against the config
/// file's own directory at parse time, so every consumer downstream deals in
/// absolute paths. A relative path that silently resolved against the process
/// CWD would make the same config behave differently depending on where the
/// operator happened to run the binary from.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub host: HostBlock,
    pub guest: GuestBlock,
    pub runtime: RuntimeBlock,
    /// Interface name -> absolute path of the bridge component.
    pub bridges: BTreeMap<String, PathBuf>,
    /// Every other top-level block, kept verbatim so concrete hosts can parse
    /// their own (`[server]`) and so `clean:host/config` can expose
    /// bridge-capability blocks (`[session]`, `[data]`, ...) per CLNH-16.
    pub extra: BTreeMap<String, toml::Value>,
    /// Directory containing the config file. Relative paths resolve against it.
    pub base_dir: PathBuf,
    /// The config file itself.
    pub source_path: PathBuf,
    /// Non-fatal problems found while parsing. CLNH-15 makes unknown keys
    /// inside reserved blocks a warning, not an error.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct HostBlock {
    pub name: String,
    pub version: String,
    pub component_model: String,
    pub deployment: Option<String>,
    pub deployment_mode: DeploymentMode,
    pub capabilities_manifest_path: Option<PathBuf>,
    pub log_level: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentMode {
    Development,
    Staging,
    Production,
}

impl DeploymentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }
}

#[derive(Debug, Clone)]
pub struct GuestBlock {
    pub name: String,
    /// Absolute path to the guest component.
    pub wasm: PathBuf,
    /// The world the guest is validated against at load time (Platform 16).
    pub world: String,
    pub mount: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeBlock {
    pub memory_max: u64,
    pub epoch_interval: Duration,
    pub instances_min: u32,
    pub instances_max: u32,
    pub instance_idle: Duration,
    pub pooling: bool,
    pub filesystem_root: Option<PathBuf>,
    pub socket_allowlist: Vec<String>,
    pub http_allowlist: Vec<String>,
    pub composed_cache: Option<PathBuf>,
}

impl HostConfig {
    /// Read and validate a `host.toml` from disk.
    ///
    /// CLNH-13: read once at startup; a missing required key is a startup error.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, HostError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|e| {
            HostError::Config(format!("cannot read config file `{}`: {e}", path.display()))
        })?;
        Self::parse(&text, path)
    }

    /// Parse config text that came from `source_path`.
    pub fn parse(text: &str, source_path: impl AsRef<Path>) -> Result<Self, HostError> {
        let source_path = source_path.as_ref();
        let base_dir = source_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let raw: RawConfig = toml::from_str(text)
            .map_err(|e| HostError::Config(format!("{}: {e}", source_path.display())))?;

        let mut warnings = Vec::new();

        let raw_host = raw
            .host
            .ok_or_else(|| HostError::Config("missing required `[host]` block".into()))?;
        let raw_guest = raw
            .guest
            .ok_or_else(|| HostError::Config("missing required `[guest]` block".into()))?;

        collect_unknown("host", &raw_host.unknown, &mut warnings);
        collect_unknown("guest", &raw_guest.unknown, &mut warnings);

        let deployment_mode = match raw_host.deployment_mode.as_deref() {
            None => DeploymentMode::Production,
            Some("development") => DeploymentMode::Development,
            Some("staging") => DeploymentMode::Staging,
            Some("production") => DeploymentMode::Production,
            Some(other) => {
                return Err(HostError::Config(format!(
                    "[host] deployment-mode must be \"development\", \"staging\", or \
                     \"production\"; got `{other}`"
                )))
            }
        };

        let host = HostBlock {
            name: require(raw_host.name, "[host] name")?,
            version: require(raw_host.version, "[host] version")?,
            component_model: require(raw_host.component_model, "[host] component-model")?,
            deployment: raw_host.deployment,
            deployment_mode,
            capabilities_manifest_path: raw_host
                .capabilities_manifest_path
                .map(|p| resolve(&base_dir, p)),
            log_level: raw_host.log_level.unwrap_or_else(|| "info".to_string()),
        };

        let guest = GuestBlock {
            name: require(raw_guest.name, "[guest] name")?,
            wasm: resolve(&base_dir, require(raw_guest.wasm, "[guest] wasm")?),
            world: require(raw_guest.world, "[guest] world")?,
            mount: raw_guest.mount.unwrap_or_else(|| "/".to_string()),
        };

        let raw_runtime = raw.runtime.unwrap_or_default();
        collect_unknown("runtime", &raw_runtime.unknown, &mut warnings);

        let runtime = RuntimeBlock {
            memory_max: match raw_runtime.memory_max {
                Some(s) => parse_size(&s).map_err(|e| cfg_err("[runtime] memory-max", &e))?,
                None => 512 * 1024 * 1024,
            },
            epoch_interval: match raw_runtime.epoch_interval {
                Some(s) => {
                    parse_duration(&s).map_err(|e| cfg_err("[runtime] epoch-interval", &e))?
                }
                None => Duration::from_millis(10),
            },
            instances_min: raw_runtime.instances_min.unwrap_or(8),
            instances_max: raw_runtime.instances_max.unwrap_or(128),
            instance_idle: match raw_runtime.instance_idle {
                Some(s) => {
                    parse_duration(&s).map_err(|e| cfg_err("[runtime] instance-idle", &e))?
                }
                None => Duration::from_secs(30),
            },
            pooling: raw_runtime.pooling.unwrap_or(true),
            // CLNH-34: no filesystem root by default.
            filesystem_root: raw_runtime.filesystem_root.map(|p| resolve(&base_dir, p)),
            // CLNH-35 / CLNH-36 defaults.
            socket_allowlist: raw_runtime
                .socket_allowlist
                .unwrap_or_else(|| vec!["*:443".to_string(), "*:80".to_string()]),
            http_allowlist: raw_runtime.http_allowlist.unwrap_or_default(),
            composed_cache: raw_runtime.composed_cache.map(|p| resolve(&base_dir, p)),
        };

        if runtime.instances_min > runtime.instances_max {
            return Err(HostError::Config(format!(
                "[runtime] instances-min ({}) exceeds instances-max ({})",
                runtime.instances_min, runtime.instances_max
            )));
        }
        if runtime.instances_max == 0 {
            return Err(HostError::Config(
                "[runtime] instances-max must be at least 1".into(),
            ));
        }

        let bridges = raw
            .bridges
            .unwrap_or_default()
            .into_iter()
            .map(|(iface, p)| (iface, resolve(&base_dir, p)))
            .collect();

        Ok(HostConfig {
            host,
            guest,
            runtime,
            bridges,
            extra: raw.extra,
            base_dir,
            source_path: source_path.to_path_buf(),
            warnings,
        })
    }

    /// Where the capability manifest is written.
    ///
    /// CLNH-45: defaults to sitting alongside the config file.
    pub fn manifest_path(&self) -> PathBuf {
        self.host
            .capabilities_manifest_path
            .clone()
            .unwrap_or_else(|| self.base_dir.join("host-capabilities.cln"))
    }

    /// A concrete host's own block (`[server]`, `[worker]`, ...), if present.
    ///
    /// CLNH-12: the library does not interpret these; it only carries them.
    pub fn host_block(&self, name: &str) -> Option<&toml::Value> {
        self.extra.get(name)
    }
}

fn require<T>(value: Option<T>, what: &str) -> Result<T, HostError> {
    value.ok_or_else(|| HostError::Config(format!("missing required key `{what}`")))
}

fn cfg_err(key: &str, msg: &str) -> HostError {
    HostError::Config(format!("{key}: {msg}"))
}

fn resolve(base: &Path, p: String) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

fn collect_unknown(block: &str, unknown: &BTreeMap<String, toml::Value>, out: &mut Vec<String>) {
    for key in unknown.keys() {
        // CLNH-15: forward compatibility — warn, don't fail.
        out.push(format!("unknown key `{key}` in `[{block}]` (ignored)"));
    }
}

/// Parse a size string: bare bytes, or a `K`/`M`/`G` suffix (binary multiples).
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size".into());
    }
    let (digits, mult) = match s.chars().last().unwrap().to_ascii_uppercase() {
        'K' => (&s[..s.len() - 1], 1024),
        'M' => (&s[..s.len() - 1], 1024 * 1024),
        'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    digits
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("`{s}` is not a valid size (expected e.g. `512M`, `10M`, `65536`)"))?
        .checked_mul(mult)
        .ok_or_else(|| format!("size `{s}` overflows"))
}

/// Parse a duration string: `ms`, `s`, `m`, or `h` suffix.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let invalid =
        || format!("`{s}` is not a valid duration (expected e.g. `30s`, `10ms`, `5m`, `1h`)");

    let (digits, unit) = if let Some(d) = s.strip_suffix("ms") {
        (d, "ms")
    } else if let Some(d) = s.strip_suffix('s') {
        (d, "s")
    } else if let Some(d) = s.strip_suffix('m') {
        (d, "m")
    } else if let Some(d) = s.strip_suffix('h') {
        (d, "h")
    } else {
        return Err(invalid());
    };

    let n: u64 = digits.trim().parse().map_err(|_| invalid())?;
    Ok(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        _ => Duration::from_secs(n * 3600),
    })
}

// ---------------------------------------------------------------------------
// Raw serde shapes. Kept private; `HostConfig` is the public surface.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawConfig {
    host: Option<RawHost>,
    guest: Option<RawGuest>,
    runtime: Option<RawRuntime>,
    bridges: Option<BTreeMap<String, String>>,
    /// CLNH-14: unknown top-level blocks are ignored by this library and left
    /// for the concrete host.
    #[serde(flatten)]
    extra: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawHost {
    name: Option<String>,
    version: Option<String>,
    component_model: Option<String>,
    deployment: Option<String>,
    deployment_mode: Option<String>,
    capabilities_manifest_path: Option<String>,
    log_level: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct RawGuest {
    name: Option<String>,
    wasm: Option<String>,
    world: Option<String>,
    mount: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct RawRuntime {
    memory_max: Option<String>,
    epoch_interval: Option<String>,
    instances_min: Option<u32>,
    instances_max: Option<u32>,
    instance_idle: Option<String>,
    pooling: Option<bool>,
    filesystem_root: Option<String>,
    socket_allowlist: Option<Vec<String>>,
    http_allowlist: Option<Vec<String>>,
    composed_cache: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `world` line inside `MINIMAL`. Kept as a constant so the
    /// "missing world" test below cannot silently stop removing it when the
    /// value changes (CMOD-01 moved it from `clean:host/server@0.1` to the
    /// bare world name `server`).
    const MINIMAL_WORLD_LINE: &str = "world = \"server\"";

    const MINIMAL: &str = r#"
[host]
name = "clean-server"
version = "0.1.0"
component-model = "0.3.0"

[guest]
name = "app"
wasm = "./dist/app.wasm"
world = "server"
"#;

    fn parse(text: &str) -> Result<HostConfig, HostError> {
        HostConfig::parse(text, "/etc/clean/host.toml")
    }

    #[test]
    fn minimal_config_applies_documented_defaults() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.host.name, "clean-server");
        assert_eq!(cfg.host.deployment_mode, DeploymentMode::Production);
        assert_eq!(cfg.guest.mount, "/");
        assert_eq!(cfg.runtime.instances_min, 8);
        assert_eq!(cfg.runtime.instances_max, 128);
        assert_eq!(cfg.runtime.memory_max, 512 * 1024 * 1024);
        assert_eq!(cfg.runtime.epoch_interval, Duration::from_millis(10));
        assert!(cfg.runtime.pooling);
        // CLNH-34/35/36: safe sandbox defaults.
        assert!(cfg.runtime.filesystem_root.is_none());
        assert_eq!(cfg.runtime.socket_allowlist, vec!["*:443", "*:80"]);
        assert!(cfg.runtime.http_allowlist.is_empty());
    }

    #[test]
    fn relative_paths_resolve_against_the_config_file_not_the_cwd() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.guest.wasm, PathBuf::from("/etc/clean/./dist/app.wasm"));
        assert_eq!(
            cfg.manifest_path(),
            PathBuf::from("/etc/clean/host-capabilities.cln")
        );
    }

    #[test]
    fn missing_required_blocks_are_startup_errors() {
        let err = parse("[guest]\nname = \"a\"\nwasm = \"a.wasm\"\nworld = \"w\"").unwrap_err();
        assert!(err.to_string().contains("[host]"), "{err}");

        let err = parse("[host]\nname = \"s\"\nversion = \"1\"\ncomponent-model = \"0.3.0\"")
            .unwrap_err();
        assert!(err.to_string().contains("[guest]"), "{err}");
    }

    #[test]
    fn missing_required_key_names_the_key() {
        let text = MINIMAL.replace(MINIMAL_WORLD_LINE, "");
        assert!(
            text != MINIMAL,
            "the world line was not removed — MINIMAL_WORLD_LINE has drifted from \
             MINIMAL, so this test would assert nothing"
        );
        let err = parse(&text).unwrap_err();
        assert!(err.to_string().contains("[guest] world"), "{err}");
    }

    #[test]
    fn unknown_key_in_reserved_block_warns_but_parses() {
        let text = format!("{MINIMAL}\n[runtime]\nfuture-key = 3\n");
        let cfg = parse(&text).unwrap();
        assert!(
            cfg.warnings.iter().any(|w| w.contains("future-key")),
            "{:?}",
            cfg.warnings
        );
    }

    #[test]
    fn unknown_top_level_block_is_carried_for_the_concrete_host() {
        let text = format!("{MINIMAL}\n[server]\nlisten = \"0.0.0.0:3000\"\n");
        let cfg = parse(&text).unwrap();
        // CLNH-14: ignored here, available to clean-server.
        assert!(cfg.host_block("server").is_some());
        assert!(cfg.warnings.is_empty(), "{:?}", cfg.warnings);
    }

    #[test]
    fn bridge_paths_resolve_and_keep_their_interface_keys() {
        let text =
            format!("{MINIMAL}\n[bridges]\n\"clean:session/store\" = \"./bridges/s.wasm\"\n");
        let cfg = parse(&text).unwrap();
        assert_eq!(
            cfg.bridges.get("clean:session/store").unwrap(),
            &PathBuf::from("/etc/clean/./bridges/s.wasm")
        );
    }

    #[test]
    fn instances_min_above_max_is_rejected() {
        let text = format!("{MINIMAL}\n[runtime]\ninstances-min = 10\ninstances-max = 4\n");
        let err = parse(&text).unwrap_err();
        assert!(err.to_string().contains("instances-min"), "{err}");
    }

    #[test]
    fn bad_deployment_mode_is_rejected_with_the_valid_set() {
        let text = MINIMAL.replace(
            "component-model = \"0.3.0\"",
            "component-model = \"0.3.0\"\ndeployment-mode = \"prod\"",
        );
        let err = parse(&text).unwrap_err();
        assert!(err.to_string().contains("production"), "{err}");
    }

    #[test]
    fn sizes_and_durations_parse_their_documented_forms() {
        assert_eq!(parse_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("10M").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("65536").unwrap(), 65536);
        assert!(parse_size("banana").is_err());

        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("10ms").unwrap(), Duration::from_millis(10));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("30").is_err());
    }
}
