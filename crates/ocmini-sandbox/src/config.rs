use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

mod paths;
pub use paths::PathContext;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selection {
    pub preset: String,
}
impl Default for Selection {
    fn default() -> Self {
        Self {
            preset: "off".into(),
        }
    }
}
impl Selection {
    pub fn name(&self) -> &str {
        &self.preset
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fs {
    pub readable: Vec<PathBuf>,
    pub writable: Vec<PathBuf>,
    pub exec: Vec<PathBuf>,
    pub hide: Vec<PathBuf>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Env {
    pub clear: bool,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessIsolation {
    #[default]
    None,
    Signal,
    Full,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnixSockets {
    Off,
    #[default]
    Abstract,
    Deny,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetMode {
    Host,
    OutboundOnly,
    InboundOnly,
    #[default]
    Isolated,
    Firewall,
}
impl NetMode {
    pub fn mediated(self) -> bool {
        matches!(
            self,
            Self::OutboundOnly | Self::InboundOnly | Self::Firewall
        )
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ech {
    #[default]
    Allow,
    Deny,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewMode {
    #[default]
    AutoLoopback,
    Off,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub mode: PreviewMode,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    #[default]
    Deny,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
    Icmp,
    #[default]
    Any,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default)]
    pub protocol: Protocol,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub port: Option<u16>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Direction {
    pub default: Action,
    #[serde(default)]
    pub allow: Vec<Rule>,
    #[serde(default)]
    pub deny: Vec<Rule>,
}
impl Direction {
    pub fn new(default: Action) -> Self {
        Self {
            default,
            allow: vec![],
            deny: vec![],
        }
    }
    pub fn permits(
        &self,
        protocol: Protocol,
        address: IpAddr,
        port: u16,
        name: Option<&str>,
    ) -> bool {
        let matches = |r: &&Rule| r.matches(protocol, address, port, name);
        !self.deny.iter().any(|r| matches(&r))
            && (self.allow.iter().any(|r| matches(&r)) || self.default == Action::Allow)
    }
    pub fn dns_permitted(&self, name: &str) -> bool {
        let matches = |r: &Rule| {
            r.domain()
                .is_some_and(|pattern| domain_matches(pattern, name))
        };
        !self.deny.iter().any(matches)
            && (self.default == Action::Allow || self.allow.iter().any(matches))
    }
    pub fn inspects_tls(&self) -> bool {
        self.allow
            .iter()
            .chain(&self.deny)
            .any(|r| r.domain().is_some())
    }
}
impl Rule {
    pub fn address(&self) -> Option<&str> {
        self.destination.as_deref().or(self.source.as_deref())
    }
    pub fn domain(&self) -> Option<&str> {
        self.address().filter(|a| parse_cidr(a).is_err())
    }
    pub fn matches(&self, protocol: Protocol, ip: IpAddr, port: u16, name: Option<&str>) -> bool {
        if self.protocol != Protocol::Any && self.protocol != protocol {
            return false;
        }
        if self.port.is_some_and(|p| p != port) {
            return false;
        }
        self.address()
            .is_none_or(|address| match parse_cidr(address) {
                Ok((network, prefix)) => cidr_contains(network, prefix, ip),
                Err(_) => name.is_some_and(|name| domain_matches(address, name)),
            })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub mode: NetMode,
    #[serde(default)]
    pub ech: Ech,
    #[serde(default)]
    pub preview: Preview,
    pub inbound: Direction,
    pub outbound: Direction,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Buffers {
    pub tcp_recv: usize,
    pub tcp_send: usize,
    pub udp_ring: usize,
}
impl Default for Buffers {
    fn default() -> Self {
        Self {
            tcp_recv: 4 * 1024 * 1024,
            tcp_send: 4 * 1024 * 1024,
            udp_ring: 64,
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Policy {
    pub name: String,
    pub fs: Fs,
    pub tmp: bool,
    pub process_isolation: ProcessIsolation,
    pub unix_sockets: UnixSockets,
    pub io_uring: bool,
    pub env: Env,
    pub network: Network,
    pub buffers: Buffers,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(rename = "extends")]
    _extends: Option<String>,
    off: Option<bool>,
    fs: Option<Fs>,
    tmp: Option<bool>,
    process_isolation: Option<ProcessIsolation>,
    pidns: Option<bool>,
    unix_sockets: Option<UnixSockets>,
    io_uring: Option<bool>,
    env: Option<Env>,
    network: Option<RawNetwork>,
    buffers: Option<Buffers>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    mode: NetMode,
    ech: Option<Ech>,
    preview: Option<Preview>,
    inbound: Option<Direction>,
    outbound: Option<Direction>,
}

pub fn load(folder: &Path, selection: &str) -> Result<Option<Policy>, String> {
    let directory = std::env::current_dir().map_err(|e| e.to_string())?;
    load_for(folder, selection, &PathContext::current(&directory)?)
}

/// Resolve templates after inheritance, against this session's trusted context.
pub fn load_for(
    folder: &Path,
    selection: &str,
    context: &PathContext,
) -> Result<Option<Policy>, String> {
    valid_name(selection)?;
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(folder).map_err(|e| format!("{}: {e}", folder.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !entry.file_type().map_err(|e| e.to_string())?.is_file()
            || path.extension().and_then(|s| s.to_str()) != Some("json")
        {
            return Err(format!(
                "Invalid preset file: {} (expected a JSON file)",
                path.display()
            ));
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("Invalid preset filename")?
            .to_owned();
        valid_name(&name)?;
        let value: Value =
            serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        let _: Raw =
            serde_json::from_value(value.clone()).map_err(|e| format!("Preset {name}: {e}"))?;
        if files.insert(name.clone(), value).is_some() {
            return Err(format!("Duplicate preset name: {name}"));
        }
    }
    // Validate the entire folder, including presets not currently selected.
    let mut selected = None;
    for name in files.keys() {
        let value = resolve(name, &files, &mut vec![])?;
        let policy = validate(name, value, context)?;
        if name == selection {
            selected = Some(policy);
        }
    }
    selected.ok_or_else(|| format!("Preset file not found: {selection}"))
}
fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(format!("Invalid preset name: {name}"));
    }
    Ok(())
}
fn resolve(
    name: &str,
    files: &BTreeMap<String, Value>,
    visiting: &mut Vec<String>,
) -> Result<Value, String> {
    if visiting.iter().any(|n| n == name) {
        visiting.push(name.into());
        return Err(format!("Preset extends cycle: {}", visiting.join(" -> ")));
    }
    let mut child = files
        .get(name)
        .cloned()
        .ok_or_else(|| format!("Preset file not found: {name}"))?;
    visiting.push(name.into());
    if let Some(parent) = child
        .get("extends")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        valid_name(&parent)?;
        let mut base = resolve(&parent, files, visiting)?;
        for (key, value) in child
            .as_object_mut()
            .ok_or("Preset must be an object")?
            .iter()
        {
            if key == "extends" {
                continue;
            }
            if key == "fs" {
                let fs = base
                    .as_object_mut()
                    .unwrap()
                    .entry("fs")
                    .or_insert(json!({}));
                for (key, values) in value.as_object().ok_or("fs must be an object")? {
                    let list = fs
                        .as_object_mut()
                        .ok_or("fs must be an object")?
                        .entry(key)
                        .or_insert(json!([]));
                    for v in values.as_array().ok_or("fs paths must be arrays")? {
                        if !list
                            .as_array()
                            .ok_or("fs paths must be arrays")?
                            .contains(v)
                        {
                            list.as_array_mut().unwrap().push(v.clone());
                        }
                    }
                }
            } else {
                base[key] = value.clone();
            }
        }
        child = base;
    }
    visiting.pop();
    child
        .as_object_mut()
        .ok_or("Preset must be an object")?
        .remove("extends");
    Ok(child)
}
fn validate(name: &str, value: Value, context: &PathContext) -> Result<Option<Policy>, String> {
    let raw: Raw =
        serde_json::from_value(value.clone()).map_err(|e| format!("Preset {name}: {e}"))?;
    if raw.off == Some(true) || name == "off" {
        if value
            .as_object()
            .is_none_or(|o| o.keys().any(|k| k != "off"))
        {
            return Err(format!("Preset {name}: policy keys in off preset"));
        }
        return Ok(None);
    }
    if raw.off.is_some() {
        return Err(format!("Preset {name}: off must be true or omitted"));
    }
    if raw.pidns.is_some() && raw.process_isolation.is_some() {
        return Err("pidns and process_isolation cannot both be set".into());
    }
    let process_isolation = raw.process_isolation.unwrap_or(if raw.pidns == Some(true) {
        ProcessIsolation::Full
    } else {
        ProcessIsolation::None
    });
    let mut fs = raw.fs.unwrap_or_default();
    context
        .resolve(&mut fs)
        .map_err(|e| format!("Preset {name}: {e}"))?;
    let covered = |p: &Path, roots: &[PathBuf]| roots.iter().any(|root| p.starts_with(root));
    for path in &fs.exec {
        if !covered(path, &fs.readable) {
            return Err(format!(
                "Preset {name}: exec root must also be readable: {}",
                path.display()
            ));
        }
    }
    for path in &fs.hide {
        if process_isolation != ProcessIsolation::Full {
            return Err(format!(
                "Preset {name}: fs.hide requires process_isolation full"
            ));
        }
        if !covered(path, &fs.readable) && !covered(path, &fs.writable) {
            return Err(format!(
                "Preset {name}: hide outside fence: {}",
                path.display()
            ));
        }
    }
    let env = raw
        .env
        .ok_or_else(|| format!("Preset {name}: env.clear is required"))?;
    for pattern in env.allow.iter().chain(&env.deny) {
        glob::Pattern::new(pattern)
            .map_err(|e| format!("Invalid environment glob {pattern}: {e}"))?;
        if pattern.contains(['=', '\0']) {
            return Err("Invalid environment name glob".into());
        }
    }
    let net = raw
        .network
        .ok_or_else(|| format!("Preset {name}: network.mode is required"))?;
    if net.mode == NetMode::Host
        && (net.preview.is_some()
            || net.inbound.is_some()
            || net.outbound.is_some()
            || net.ech.is_some())
    {
        return Err(format!(
            "Preset {name}: host mode refuses preview, ech and rules"
        ));
    }
    if net.mode == NetMode::Isolated && net.ech.is_some() {
        return Err(format!("Preset {name}: isolated mode refuses ech"));
    }
    let (incoming, outgoing) = match net.mode {
        NetMode::Host => (Action::Allow, Action::Allow),
        NetMode::OutboundOnly => (Action::Deny, Action::Allow),
        NetMode::InboundOnly => (Action::Allow, Action::Deny),
        NetMode::Isolated | NetMode::Firewall => (Action::Deny, Action::Deny),
    };
    if net.mode == NetMode::Firewall && (net.inbound.is_none() || net.outbound.is_none()) {
        return Err(format!(
            "Preset {name}: firewall requires inbound.default and outbound.default"
        ));
    }
    if net.mode == NetMode::Isolated && (net.inbound.is_some() || net.outbound.is_some()) {
        return Err(format!("Preset {name}: isolated mode refuses rules"));
    }
    let mut network = Network {
        mode: net.mode,
        ech: net.ech.unwrap_or_default(),
        preview: net.preview.unwrap_or_default(),
        inbound: net.inbound.unwrap_or_else(|| Direction::new(incoming)),
        outbound: net.outbound.unwrap_or_else(|| Direction::new(outgoing)),
    };
    for (outbound, direction) in [(false, &mut network.inbound), (true, &mut network.outbound)] {
        for rule in direction.allow.iter_mut().chain(&mut direction.deny) {
            if (outbound && rule.source.is_some()) || (!outbound && rule.destination.is_some()) {
                return Err("Inbound rules use source; outbound rules use destination".into());
            }
            if rule.port == Some(0) || (rule.protocol == Protocol::Icmp && rule.port.is_some()) {
                return Err("Invalid rule port (ICMP rules have no ports)".into());
            }
            if let Some(address) = rule.destination.as_mut().or(rule.source.as_mut()) {
                if parse_cidr(address).is_err() {
                    if address.contains('/')
                        || !outbound
                        || rule.protocol != Protocol::Tcp
                        || rule.port != Some(443)
                    {
                        return Err(format!(
                            "Invalid domain rule: {address}; domains require outbound tcp/443"
                        ));
                    }
                    *address = normalize_pattern(address)?;
                }
            }
        }
    }
    let buffers = raw.buffers.unwrap_or_default();
    if buffers.tcp_recv < 1500 || buffers.tcp_send < 1500 || buffers.udp_ring == 0 {
        return Err(format!(
            "Preset {name}: invalid buffers (TCP >= 1500, UDP > 0)"
        ));
    }
    Ok(Some(Policy {
        name: name.into(),
        fs,
        tmp: raw.tmp.unwrap_or(false),
        process_isolation,
        unix_sockets: raw.unix_sockets.unwrap_or_default(),
        io_uring: raw.io_uring.unwrap_or(true),
        env,
        network,
        buffers,
    }))
}
pub fn normalize_name(name: &str) -> Result<String, String> {
    let name = idna::domain_to_ascii_strict(name.trim_end_matches('.'))
        .map_err(|_| "Invalid hostname")?
        .to_ascii_lowercase();
    if name.is_empty()
        || name.len() > 253
        || name.split('.').any(|s| {
            s.is_empty()
                || s.len() > 63
                || s.starts_with('-')
                || s.ends_with('-')
                || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err("Invalid hostname".into());
    }
    Ok(name)
}
fn normalize_pattern(name: &str) -> Result<String, String> {
    match name.strip_prefix("*.") {
        Some(suffix) => Ok(format!("*.{}", normalize_name(suffix)?)),
        None => normalize_name(name),
    }
}
pub fn domain_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => name
            .strip_suffix(suffix)
            .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1),
        None => pattern == name,
    }
}
pub fn parse_cidr(address: &str) -> Result<(IpAddr, u8), String> {
    let (ip, prefix) = address
        .split_once('/')
        .map_or((address, None), |(ip, prefix)| (ip, Some(prefix)));
    let ip: IpAddr = ip.parse().map_err(|_| "Invalid IP/CIDR")?;
    let bits = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = match prefix {
        Some(s) => s.parse::<u8>().map_err(|_| "Invalid CIDR prefix")?,
        None => bits,
    };
    if prefix > bits {
        return Err("Invalid CIDR prefix".into());
    }
    Ok((ip, prefix))
}
fn cidr_contains(network: IpAddr, prefix: u8, ip: IpAddr) -> bool {
    match (network, ip) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            prefix == 0 || (u32::from(a) >> (32 - prefix)) == (u32::from(b) >> (32 - prefix))
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            prefix == 0 || (u128::from(a) >> (128 - prefix)) == (u128::from(b) >> (128 - prefix))
        }
        _ => false,
    }
}
impl Policy {
    pub fn environment(
        &self,
        inherited: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    ) -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
        let mut env = BTreeMap::new();
        for (key, value) in inherited {
            let name = key.to_string_lossy();
            let matches = |patterns: &[String]| {
                patterns
                    .iter()
                    .any(|p| glob::Pattern::new(p).is_ok_and(|p| p.matches(&name)))
            };
            if (!self.env.clear || matches(&self.env.allow))
                && !matches(&self.env.deny)
                && name != "PATH"
            {
                env.insert(key, value);
            }
        }
        let roots: BTreeSet<_> = self.fs.exec.iter().map(|p| p.as_os_str()).collect();
        env.insert(
            "PATH".into(),
            std::env::join_paths(roots).unwrap_or_default(),
        );
        env
    }
}

/// Install once using a synced directory rename. Deleting a preset never recreates it.
pub fn initialize(folder: &Path) -> Result<(), String> {
    if folder.exists() {
        return Ok(());
    }
    let parent = folder.parent().ok_or("Invalid preset folder")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = parent.join(format!(
        ".presets-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&staging).map_err(|e| e.to_string())?;
    let result = (|| {
        for name in [
            "off",
            "read-only",
            "workspace-write",
            "strict",
            "locked-down",
        ] {
            let value = if name == "off" {
                json!({"off":true})
            } else {
                let mode = match name {
                    "workspace-write" => "firewall",
                    "strict" => "outbound-only",
                    _ => "isolated",
                };
                let mut net = json!({"mode":mode,"preview":{"mode":if mode == "isolated" {"off"} else {"auto-loopback"}}});
                if mode == "firewall" {
                    net["inbound"] = json!({"default":"allow"});
                    net["outbound"] = json!({"default":"allow"});
                }
                json!({"fs":{"readable":["${system_readable}","${toolchains}","${workspace}"],"writable":if name=="read-only" {vec![]} else {vec!["${workspace}"]},"exec":["${system_exec}","${toolchains}"]},"tmp":true,"process_isolation":if name=="strict" || name=="locked-down" {"full"} else {"none"},"unix_sockets":"abstract","io_uring":true,"env":{"clear":true,"allow":["HOME","USER","TERM","COLORTERM","LANG","LC_*","TZ"],"deny":[]},"network":net})
            };
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(staging.join(format!("{name}.json")))
                .map_err(|e| e.to_string())?;
            file.write_all(&serde_json::to_vec_pretty(&value).unwrap())
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
        }
        std::fs::File::open(&staging)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::rename(&staging, folder) {
            if !folder.exists() {
                return Err(e.to_string());
            }
        }
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn validate(name: &str, value: Value) -> Result<Option<Policy>, String> {
        super::validate(
            name,
            value,
            &PathContext::current(Path::new("/workspace")).unwrap(),
        )
    }
    fn base() -> Value {
        json!({"fs":{"readable":["/usr"],"writable":[],"exec":["/usr/bin"]},"env":{"clear":true},"network":{"mode":"isolated"}})
    }
    #[test]
    fn schema_refuses_unknown_missing_and_incompatible_keys() {
        let mut cases = vec![];
        let mut p = base();
        p["unknown"] = true.into();
        cases.push(p);
        let mut p = base();
        p["env"] = json!({});
        cases.push(p);
        let mut p = base();
        p["unix_sockets"] = "deny-no-uring".into();
        cases.push(p);
        let mut p = base();
        p["network"]["ech"] = "deny".into();
        cases.push(p);
        let mut p = base();
        p["network"] = json!({"mode":"host","preview":{"mode":"off"}});
        cases.push(p);
        let mut p = base();
        p["fs"]["hide"] = json!(["/usr/secret"]);
        cases.push(p);
        let mut p = base();
        p["process_isolation"] = "full".into();
        p["fs"]["hide"] = json!(["/outside"]);
        cases.push(p);
        let mut p = base();
        p["fs"]["readable"] = json!(["/usr/*"]);
        cases.push(p);
        let mut p = base();
        p["buffers"] = json!({"tcp_recv":1499});
        cases.push(p);
        for case in cases {
            assert!(validate("test", case.clone()).is_err(), "{case}");
        }
        assert!(validate("off", base()).unwrap_err().contains("policy keys"));
        let mut p = base();
        p["io_uring"] = false.into();
        p["unix_sockets"] = "off".into();
        assert!(!validate("test", p).unwrap().unwrap().io_uring);
    }
    #[test]
    fn inheritance_unions_paths_and_overrides_blocks_and_names_cycles() {
        let mut child =
            json!({"extends":"parent","fs":{"readable":["/opt"]},"env":{"clear":false}});
        let mut files =
            BTreeMap::from([("parent".into(), base()), ("child".into(), child.clone())]);
        let merged = resolve("child", &files, &mut vec![]).unwrap();
        assert_eq!(merged["fs"]["readable"], json!(["/usr", "/opt"]));
        assert_eq!(merged["env"], json!({"clear":false}));
        child["extends"] = "loop".into();
        files.insert("child".into(), child);
        files.insert("loop".into(), json!({"extends":"child"}));
        assert!(resolve("child", &files, &mut vec![])
            .unwrap_err()
            .contains("child -> loop -> child"));
    }
    #[test]
    fn deny_precedence_domain_boundaries_and_dns_authorization() {
        let mut direction = Direction::new(Action::Deny);
        direction.allow.push(Rule {
            protocol: Protocol::Tcp,
            source: None,
            destination: Some("*.example.org".into()),
            port: Some(443),
        });
        direction.deny.push(Rule {
            protocol: Protocol::Tcp,
            source: None,
            destination: Some("blocked.example.org".into()),
            port: Some(443),
        });
        assert!(direction.dns_permitted("api.example.org"));
        assert!(!direction.dns_permitted("example.org"));
        assert!(!direction.dns_permitted("evilexample.org"));
        assert!(!direction.dns_permitted("blocked.example.org"));
        assert!(direction.permits(
            Protocol::Tcp,
            "192.0.2.1".parse().unwrap(),
            443,
            Some("api.example.org")
        ));
        direction.deny.push(Rule {
            protocol: Protocol::Any,
            source: None,
            destination: Some("192.0.2.0/24".into()),
            port: None,
        });
        assert!(!direction.permits(
            Protocol::Tcp,
            "192.0.2.1".parse().unwrap(),
            443,
            Some("api.example.org")
        ));
        assert_eq!(
            normalize_name("BÜCHER.Example.").unwrap(),
            "xn--bcher-kva.example"
        );
    }
    #[test]
    fn folder_validation_includes_unselected_presets_and_deletion_is_persistent() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("presets");
        initialize(&folder).unwrap();
        assert!(load(&folder, "off").unwrap().is_none());
        std::fs::remove_file(folder.join("strict.json")).unwrap();
        initialize(&folder).unwrap();
        assert!(load(&folder, "strict").is_err());
        std::fs::write(folder.join("broken.json"), "[]").unwrap();
        assert!(load(&folder, "off").is_err());
    }
    #[test]
    fn one_preset_resolves_each_workspace_and_user_after_inheritance() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("presets");
        std::fs::create_dir(&folder).unwrap();
        let parent = json!({"fs":{"readable":["${workspace}","${home}"]},"env":{"clear":true},"network":{"mode":"isolated"}});
        let child = json!({"extends":"base","fs":{"writable":["${workspace}"],"exec":["${workspace}/bin"],"hide":["${home}/.ssh"]},"process_isolation":"full"});
        std::fs::write(folder.join("base.json"), parent.to_string()).unwrap();
        std::fs::write(folder.join("custom.json"), child.to_string()).unwrap();
        for user in ["alice", "bob"] {
            let mut context = PathContext::current(root.path()).unwrap();
            context.workspace = root.path().join(user).join("workspace");
            context.home = root.path().join(user).join("home");
            let policy = load_for(&folder, "custom", &context).unwrap().unwrap();
            assert_eq!(policy.fs.writable, vec![context.workspace.clone()]);
            assert_eq!(policy.fs.exec, vec![context.workspace.join("bin")]);
            assert_eq!(policy.fs.hide, vec![context.home.join(".ssh")]);
            assert_eq!(policy.fs.readable, vec![context.workspace, context.home]);
        }
        let mut invalid = child;
        invalid["fs"]["hide"] = json!(["${cache_home}/outside"]);
        std::fs::write(folder.join("custom.json"), invalid.to_string()).unwrap();
        let mut context = PathContext::current(root.path()).unwrap();
        context.workspace = root.path().join("workspace");
        context.home = root.path().join("home");
        context.cache_home = root.path().join("outside-cache");
        assert!(load_for(&folder, "base", &context)
            .unwrap_err()
            .contains("hide outside fence"));
    }
    #[test]
    fn installed_defaults_store_templates_and_resolve_without_rewriting_files() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("presets");
        initialize(&folder).unwrap();
        let before = std::fs::read(folder.join("workspace-write.json")).unwrap();
        let template: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(
            template["fs"]["readable"],
            json!(["${system_readable}", "${toolchains}", "${workspace}"])
        );
        for name in ["first", "second"] {
            let path = root.path().join(name);
            let context = PathContext::current(&path).unwrap();
            let policy = load_for(&folder, "workspace-write", &context)
                .unwrap()
                .unwrap();
            assert_eq!(policy.fs.writable, vec![path.clone()]);
            assert!(policy.fs.readable.contains(&path));
        }
        assert_eq!(
            before,
            std::fs::read(folder.join("workspace-write.json")).unwrap()
        );
    }
}
