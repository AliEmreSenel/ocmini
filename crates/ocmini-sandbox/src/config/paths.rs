//! Trusted host context for preset paths. Expansion never executes a shell and
//! never interprets text inserted by a variable as another template.
use super::Fs;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Captured in the brain for the actual session directory, before jail setup.
/// User and XDG paths describe the host process, not the runner's user namespace.
#[derive(Clone, Debug)]
pub struct PathContext {
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub user: OsString,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub cache_home: PathBuf,
}
fn env_non_empty(key: &str) -> Option<OsString> {
    std::env::var_os(key).filter(|value| !value.is_empty())
}
impl PathContext {
    pub fn current(workspace: &Path) -> Result<Self, String> {
        let home = env_non_empty("HOME").or_else(|| env_non_empty("USERPROFILE"));
        let user = env_non_empty("USER")
            .or_else(|| env_non_empty("LOGNAME"))
            .or_else(|| env_non_empty("USERNAME"));
        #[cfg(unix)]
        let (uid, gid, account) = {
            let uid = unsafe { libc::geteuid() };
            let gid = unsafe { libc::getegid() };
            let account = if home.is_none() || user.is_none() {
                account(uid)?
            } else {
                None
            };
            (Some(uid), Some(gid), account)
        };
        #[cfg(not(unix))]
        let (uid, gid, account): (Option<u32>, Option<u32>, Option<(OsString, OsString)>) =
            (None, None, None);
        let (account_user, account_home) = account.unzip();
        let home = PathBuf::from(home.or(account_home).unwrap_or_default());
        Ok(Self {
            workspace: workspace.to_owned(),
            user: user
                .or(account_user)
                .unwrap_or_else(|| uid.map(|n| n.to_string()).unwrap_or_default().into()),
            uid,
            gid,
            config_home: xdg("XDG_CONFIG_HOME", &home, ".config"),
            data_home: xdg("XDG_DATA_HOME", &home, ".local/share"),
            cache_home: xdg("XDG_CACHE_HOME", &home, ".cache"),
            home,
        })
    }
    fn scalar(&self, name: &str) -> Result<OsString, String> {
        let path = match name {
            "workspace" => Some(&self.workspace),
            "home" => Some(&self.home),
            "config_home" => Some(&self.config_home),
            "data_home" => Some(&self.data_home),
            "cache_home" => Some(&self.cache_home),
            _ => None,
        };
        if let Some(path) = path {
            validate_path(path)?;
            return Ok(path.as_os_str().to_owned());
        }
        let value = match name {
            "user" => self.user.clone(),
            "uid" => self
                .uid
                .map(|n| n.to_string())
                .ok_or("Host uid is unavailable")?
                .into(),
            "gid" => self
                .gid
                .map(|n| n.to_string())
                .ok_or("Host gid is unavailable")?
                .into(),
            _ => {
                let key = name
                    .strip_prefix("env:")
                    .filter(|key| {
                        !key.is_empty()
                            && key.bytes().enumerate().all(|(i, c)| {
                                c.is_ascii_alphabetic() || c == b'_' || i > 0 && c.is_ascii_digit()
                            })
                    })
                    .ok_or_else(|| format!("Unknown sandbox path variable: ${{{name}}}"))?;
                std::env::var_os(key)
                    .ok_or_else(|| format!("Sandbox path variable ${{env:{key}}} is unset"))?
            }
        };
        if value.is_empty() {
            return Err(format!("Sandbox path variable ${{{name}}} is empty"));
        }
        Ok(value)
    }
    fn expand(&self, template: &Path) -> Result<PathBuf, String> {
        let template = template.to_str().ok_or("Preset paths must be UTF-8")?;
        if template.contains(['*', '?', '[', '\0']) {
            return Err(format!(
                "Preset paths cannot contain globs or NUL: {template}"
            ));
        }
        let mut expanded = OsString::new();
        let mut rest = template;
        while let Some(start) = rest.find("${") {
            expanded.push(&rest[..start]);
            let variable = &rest[start + 2..];
            let end = variable
                .find('}')
                .ok_or_else(|| format!("Unclosed sandbox path variable: {template}"))?;
            let variable_name = &variable[..end];
            let value = self.scalar(variable_name)?;
            if variable_name.starts_with("env:") && std::env::split_paths(&value).nth(1).is_some() {
                return Err(format!("Path-list variable ${{{variable_name}}} must occupy an entire filesystem-list entry"));
            }
            expanded.push(value);
            rest = &variable[end + 1..];
        }
        expanded.push(rest);
        let path = PathBuf::from(expanded);
        validate_path(&path)?;
        Ok(path)
    }
    pub(super) fn resolve(&self, fs: &mut Fs) -> Result<(), String> {
        for (list, hide) in [
            (&mut fs.readable, false),
            (&mut fs.writable, false),
            (&mut fs.exec, false),
            (&mut fs.hide, true),
        ] {
            let mut expanded = Vec::new();
            for template in &*list {
                let environment = template
                    .to_str()
                    .and_then(|s| s.strip_prefix("${"))
                    .and_then(|s| s.strip_suffix('}'))
                    .filter(|s| s.starts_with("env:") && !s.contains(['{', '}']));
                if let Some(variable) = environment {
                    append_environment_paths(
                        &mut expanded,
                        variable,
                        &self.scalar(variable)?,
                        hide,
                    )?;
                    continue;
                }
                let path = self.expand(template)?;
                if !expanded.contains(&path) {
                    expanded.push(path);
                }
            }
            *list = expanded;
        }
        Ok(())
    }
}

fn append_environment_paths(
    paths: &mut Vec<PathBuf>,
    variable: &str,
    value: &std::ffi::OsStr,
    hide: bool,
) -> Result<(), String> {
    for path in std::env::split_paths(value) {
        validate_path(&path).map_err(|e| format!("Sandbox path variable ${{{variable}}}: {e}"))?;
        // Path lists may contain locations for tools that are not installed.
        // Resolve only the supplied entries; never guess additional roots.
        let exists = path
            .try_exists()
            .map_err(|e| format!("Sandbox path variable ${{{variable}}}: {e}"))?;
        if hide && !exists {
            return Err(format!(
                "Sandbox path variable ${{{variable}}}: hide target does not exist: {}",
                path.display()
            ));
        }
        if exists && !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(())
}
fn xdg(key: &str, root: &Path, suffix: &str) -> PathBuf {
    env_non_empty(key)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(suffix))
}
pub(super) fn validate_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path.as_os_str().as_encoded_bytes().contains(&0)
        || path.components().any(|c| c == Component::ParentDir)
    {
        return Err(format!(
            "Preset paths must resolve to absolute paths without NUL or ..: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn account(uid: u32) -> Result<Option<(OsString, OsString)>, String> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut size = 1024;
    loop {
        let mut buffer = vec![0u8; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let error = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE {
            size = size
                .checked_mul(2)
                .ok_or("User account record is too large")?;
            continue;
        }
        if error != 0 {
            return Err(format!(
                "Resolve current user: {}",
                std::io::Error::from_raw_os_error(error)
            ));
        }
        if result.is_null() {
            return Ok(None);
        }
        let entry = unsafe { entry.assume_init() };
        let copy = |value: *const libc::c_char| {
            if value.is_null() {
                OsString::new()
            } else {
                OsStr::from_bytes(unsafe { std::ffi::CStr::from_ptr(value) }.to_bytes()).to_owned()
            }
        };
        return Ok(Some((copy(entry.pw_name), copy(entry.pw_dir))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> PathContext {
        PathContext {
            workspace: "/projects/space [literal] ${home}".into(),
            home: "/home/alice".into(),
            user: "alice".into(),
            uid: Some(1001),
            gid: Some(1002),
            config_home: "/config/alice".into(),
            data_home: "/data/alice".into(),
            cache_home: "/cache/alice".into(),
        }
    }
    #[test]
    fn expands_host_context_once_without_interpreting_inserted_paths() {
        let ctx = context();
        for (template, expected) in [
            ("${workspace}/src", ctx.workspace.join("src")),
            ("${home}/.cargo", ctx.home.join(".cargo")),
            (
                "/run/user/${uid}/${user}-${gid}",
                PathBuf::from("/run/user/1001/alice-1002"),
            ),
            ("${config_home}/opencode", ctx.config_home.join("opencode")),
            ("${data_home}/ocmini", ctx.data_home.join("ocmini")),
            ("${cache_home}/build", ctx.cache_home.join("build")),
        ] {
            assert_eq!(ctx.expand(Path::new(template)).unwrap(), expected);
        }
    }
    #[test]
    fn refuses_missing_variables_malformed_templates_and_traversal() {
        let ctx = context();
        for template in [
            "${unknown}",
            "${system_readable}",
            "${system_exec}",
            "${toolchains}",
            "${}",
            "${workspace",
            "${env:}",
            "${env:1INVALID}",
            "${env:OCMINI_TEST_MISSING_PATH_82a8dc}",
            "${system_exec}/bin",
            "${workspace}/../outside",
            "/tmp/*",
            "/tmp/a?",
            "/tmp/a[1]",
            "relative",
        ] {
            assert!(ctx.expand(Path::new(template)).is_err(), "{template}");
        }
        let mut relative = context();
        relative.workspace = "relative".into();
        assert!(relative.expand(Path::new("/prefix/${workspace}")).is_err());
        relative.user.clear();
        assert!(relative.expand(Path::new("/prefix/${user}")).is_err());
        relative.uid = None;
        assert!(relative.expand(Path::new("/run/user/${uid}")).is_err());
    }

    #[test]
    fn environment_lists_preserve_literal_paths_and_remove_duplicate_grants() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("space [literal] ${home}");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let missing = root.path().join("not-installed");
        let value = std::env::join_paths([&first, &missing, &second, &first]).unwrap();
        let mut paths = vec![first.clone()];
        append_environment_paths(&mut paths, "env:MY_TOOLS", &value, false).unwrap();
        assert_eq!(paths, vec![first, second]);
        assert!(
            append_environment_paths(&mut vec![], "env:MY_TOOLS", &value, true)
                .unwrap_err()
                .contains("hide target does not exist")
        );
    }

    #[test]
    fn environment_lists_refuse_empty_relative_and_traversing_entries() {
        for invalid in ["", "relative", "/tools/../outside"] {
            let value = std::env::join_paths(["/valid", invalid]).unwrap();
            assert!(
                append_environment_paths(&mut vec![], "env:MY_DIRS", &value, false).is_err(),
                "{invalid}"
            );
        }
    }
}
