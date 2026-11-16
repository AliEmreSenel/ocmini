pub use ocmini_sandbox::cli::SandboxArgs;

#[cfg(test)]
mod tests {
    use clap::Parser;
    use ocmini_sandbox::config::{self, Action, NetMode, PathContext};
    use std::path::PathBuf;

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
            let root = tempfile::tempdir().unwrap();
            let folder = root.path().join("presets");
            config::initialize(&folder).unwrap();
            let context = PathContext::current(root.path()).unwrap();
            let (_, policy) = config::load_settings(
                &folder,
                "read-only",
                &context,
                &cli.sandbox.overrides().unwrap(),
            )
            .unwrap();
            assert!(
                policy
                    .unwrap()
                    .fs
                    .readable
                    .ends_with(&[PathBuf::from("/first"), PathBuf::from("/second")]),
                "{command:?}"
            );
        }
    }
}
