use clap::Args;
use ocmini_sandbox::config::Overrides;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Args, Default)]
#[group(id = "sandbox_options")]
#[command(next_help_heading = "Sandbox overrides")]
pub struct SandboxArgs {
    /// Overlay a JSON policy file (repeatable; later files override earlier files)
    #[arg(long = "sandbox-config", value_name = "FILE")]
    config: Vec<PathBuf>,
    /// Replace the network policy (firewall starts with both directions denied)
    #[arg(long = "sandbox-network", value_name = "MODE", value_parser = ["host", "outbound-only", "inbound-only", "isolated", "firewall"])]
    network: Option<String>,
    /// Replace a policy setting; VALUE is JSON or a bare string (repeatable)
    #[arg(long = "sandbox-set", value_name = "KEY=VALUE", value_parser = assignment)]
    set: Vec<(String, Value)>,
    /// Append a value or JSON array to a policy list (repeatable)
    #[arg(long = "sandbox-add", value_name = "KEY=VALUE", value_parser = assignment)]
    add: Vec<(String, Value)>,
    /// Add a readable path; supports simple path variables (repeatable)
    #[arg(long = "sandbox-read", value_name = "PATH")]
    read: Vec<String>,
    /// Add a writable path (repeatable)
    #[arg(long = "sandbox-write", value_name = "PATH")]
    write: Vec<String>,
    /// Add an executable path; it must also be readable (repeatable)
    #[arg(long = "sandbox-exec", value_name = "PATH")]
    exec: Vec<String>,
    /// Hide a granted path; requires full process isolation (repeatable)
    #[arg(long = "sandbox-hide", value_name = "PATH")]
    hide: Vec<String>,
}

impl SandboxArgs {
    // Clap propagates only one occurrence of a global argument across command
    // scopes. Combine explicit groups so repeated additions are never dropped.
    pub fn append(&mut self, mut other: Self) {
        self.config.append(&mut other.config);
        self.network = other.network.or(self.network.take());
        self.set.append(&mut other.set);
        self.add.append(&mut other.add);
        self.read.append(&mut other.read);
        self.write.append(&mut other.write);
        self.exec.append(&mut other.exec);
        self.hide.append(&mut other.hide);
    }

    pub fn overrides(&self) -> Result<Overrides, String> {
        let mut overrides = Overrides::default();
        for path in &self.config {
            let mut read = || {
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                let value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                overrides.merge(value)
            };
            read().map_err(|e| format!("Sandbox overlay {}: {e}", path.display()))?;
        }
        if let Some(mode) = &self.network {
            let mut network = json!({"mode": mode});
            if mode == "firewall" {
                network["inbound"] = json!({"default": "deny"});
                network["outbound"] = json!({"default": "deny"});
            }
            overrides.set("network", network)?;
        }
        for (key, value) in &self.set {
            overrides.set(key, value.clone())?;
        }
        for (key, value) in &self.add {
            overrides.add(key, value.clone())?;
        }
        for (key, paths) in [
            ("fs.readable", &self.read),
            ("fs.writable", &self.write),
            ("fs.exec", &self.exec),
            ("fs.hide", &self.hide),
        ] {
            for path in paths {
                overrides.add(key, path.clone().into())?;
            }
        }
        Ok(overrides)
    }
}

fn assignment(value: &str) -> Result<(String, Value), String> {
    let (key, value) = value.split_once('=').ok_or("Expected KEY=VALUE")?;
    let value = serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()));
    Ok((key.to_owned(), value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use ocmini_sandbox::config::{self, Action, NetMode, PathContext};

    #[test]
    fn cli_flags_apply_files_then_replacements_then_additions() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("presets");
        config::initialize(&folder).unwrap();
        let first = root.path().join("first.json");
        let second = root.path().join("second.json");
        std::fs::write(&first, r#"{"fs":{"readable":["/from-first"]},"tmp":false}"#).unwrap();
        std::fs::write(
            &second,
            r#"{"fs":{"readable":["/from-second"]},"tmp":true}"#,
        )
        .unwrap();
        let mut cli = crate::Cli::try_parse_from([
            "ocmini",
            "run",
            "--sandbox-config",
            first.to_str().unwrap(),
            "--sandbox-config",
            second.to_str().unwrap(),
            "--sandbox-network",
            "firewall",
            "--sandbox-set",
            "network.outbound.default=allow",
            "--sandbox-set",
            "fs.exec=[]",
            "--sandbox-set",
            "tmp=false",
            "--sandbox-read",
            "${workspace}/extra",
            "--sandbox-write",
            "${workspace}/extra",
            "--sandbox-add",
            "network.outbound.deny={\"destination\":\"10.0.0.0/8\"}",
        ])
        .unwrap();
        cli.merge_sandbox();
        let context = PathContext::current(root.path()).unwrap();
        let policy = config::load_with_overrides(
            &folder,
            "workspace-write",
            &context,
            &cli.sandbox.overrides().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            policy.fs.readable,
            vec![PathBuf::from("/from-second"), root.path().join("extra")]
        );
        assert!(policy.fs.writable.contains(&root.path().join("extra")));
        assert!(policy.fs.exec.is_empty());
        assert!(!policy.tmp);
        assert_eq!(policy.network.mode, NetMode::Firewall);
        assert_eq!(policy.network.inbound.default, Action::Deny);
        assert_eq!(policy.network.outbound.default, Action::Allow);
        assert!(!policy.network.outbound.permits(
            config::Protocol::Tcp,
            "10.0.0.1".parse().unwrap(),
            80,
            None
        ));
    }

    #[test]
    fn repeatable_global_flags_work_before_and_after_subcommands() {
        for command in [None, Some("tui"), Some("run"), Some("serve")] {
            let mut args = vec!["ocmini", "--sandbox-read", "/first"];
            args.extend(command);
            args.extend(["--sandbox-read", "/second"]);
            let mut cli = crate::Cli::try_parse_from(args).unwrap();
            cli.merge_sandbox();
            assert_eq!(cli.sandbox.read, ["/first", "/second"], "{command:?}");
        }
    }
}
