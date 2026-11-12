//! Thin layer over the system OpenSSH client (S5).
//!
//! dse never stores connection settings: it shells out to `ssh`, so keys,
//! agent, `ProxyJump`, `Include`, aliases and `known_hosts` behave exactly as
//! they already do in the user's terminal. Resolution is only used to *show*
//! the effective target, never to bypass the client.

use std::process::Command;

/// Connection knobs forwarded to every ssh invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSettings {
    pub bin: String,
    pub connect_timeout_secs: u64,
    /// Raw `-o` options, for example `StrictHostKeyChecking=accept-new`.
    pub options: Vec<String>,
}

impl Default for SshSettings {
    fn default() -> Self {
        Self {
            bin: "ssh".to_string(),
            connect_timeout_secs: 5,
            options: Vec::new(),
        }
    }
}

/// A fully built ssh command line, ready to hand to `std::process` or tokio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshInvocation {
    pub bin: String,
    pub args: Vec<String>,
}

/// Effective settings reported by `ssh -G <target>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub target: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
}

impl SshTarget {
    /// Compact `user@hostname:port` rendering, port omitted when it is 22.
    pub fn endpoint(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.hostname)
        } else {
            format!("{}@{}:{}", self.user, self.hostname, self.port)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("invalid ssh target `{0}`: must not be empty, start with '-' or contain whitespace")]
    InvalidTarget(String),
    #[error("could not run `{bin}`: {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{bin} -G {target}` failed: {stderr}")]
    Resolve {
        bin: String,
        target: String,
        stderr: String,
    },
    #[error("`{bin} -G {target}` reported no hostname")]
    NoHostname { bin: String, target: String },
}

fn check_target(target: &str) -> Result<(), SshError> {
    let invalid =
        target.is_empty() || target.starts_with('-') || target.chars().any(char::is_whitespace);
    if invalid {
        return Err(SshError::InvalidTarget(target.to_string()));
    }
    Ok(())
}

fn base_args(settings: &SshSettings) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", settings.connect_timeout_secs),
    ];
    for option in &settings.options {
        args.push("-o".to_string());
        args.push(option.clone());
    }
    args
}

impl SshInvocation {
    /// `ssh -o ... <target> <command>`, with the command passed as a single
    /// argument so the remote login shell parses it exactly as typed.
    pub fn remote(settings: &SshSettings, target: &str, command: &str) -> Result<Self, SshError> {
        check_target(target)?;
        let mut args = base_args(settings);
        args.push(target.to_string());
        args.push(command.to_string());
        Ok(Self {
            bin: settings.bin.clone(),
            args,
        })
    }

    /// `ssh -o ... -G <target>`: asks the client for the effective config.
    pub fn resolve(settings: &SshSettings, target: &str) -> Result<Self, SshError> {
        check_target(target)?;
        let mut args = base_args(settings);
        args.push("-G".to_string());
        args.push(target.to_string());
        Ok(Self {
            bin: settings.bin.clone(),
            args,
        })
    }

    pub fn output(&self) -> Result<std::process::Output, SshError> {
        Command::new(&self.bin)
            .args(&self.args)
            .output()
            .map_err(|source| SshError::Spawn {
                bin: self.bin.clone(),
                source,
            })
    }
}

/// Parse the key/value output of `ssh -G`.
pub fn parse_effective(target: &str, output: &str) -> Result<SshTarget, SshError> {
    let mut hostname = None;
    let mut user = None;
    let mut port = None;
    for line in output.lines() {
        let mut parts = line.splitn(2, ' ');
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        match key {
            "hostname" => hostname = Some(value.trim().to_string()),
            "user" => user = Some(value.trim().to_string()),
            "port" => port = value.trim().parse::<u16>().ok(),
            _ => {}
        }
    }
    let missing = |what: &str| SshError::NoHostname {
        bin: what.to_string(),
        target: target.to_string(),
    };
    Ok(SshTarget {
        target: target.to_string(),
        hostname: hostname.unwrap_or_else(|| target.to_string()),
        user: user.ok_or_else(|| missing("user"))?,
        port: port.ok_or_else(|| missing("port"))?,
    })
}

/// Resolve the effective target through the ssh client.
pub fn resolve_target(settings: &SshSettings, target: &str) -> Result<SshTarget, SshError> {
    let invocation = SshInvocation::resolve(settings, target)?;
    let output = invocation.output()?;
    if !output.status.success() {
        return Err(SshError::Resolve {
            bin: invocation.bin,
            target: target.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    parse_effective(target, &String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SshSettings {
        SshSettings {
            bin: "ssh".to_string(),
            connect_timeout_secs: 7,
            options: vec!["StrictHostKeyChecking=accept-new".to_string()],
        }
    }

    #[test]
    fn remote_invocation_is_non_interactive_and_quoted_as_one_argument() {
        let invocation =
            SshInvocation::remote(&settings(), "pve00.jacd.local", "apt update && echo ok")
                .expect("builds");
        assert_eq!(invocation.bin, "ssh");
        assert_eq!(
            invocation.args,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=7",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "pve00.jacd.local",
                "apt update && echo ok",
            ]
        );
    }

    #[test]
    fn resolve_invocation_ends_with_dash_g() {
        let invocation = SshInvocation::resolve(&settings(), "pve00.jacd.local").expect("builds");
        assert_eq!(
            invocation.args.last().map(String::as_str),
            Some("pve00.jacd.local")
        );
        assert!(invocation.args.contains(&"-G".to_string()));
    }

    #[test]
    fn rejects_option_injection_targets() {
        for target in ["-oProxyCommand=evil", "host name", ""] {
            let err = SshInvocation::remote(&settings(), target, "true").expect_err("rejects");
            assert!(err.to_string().starts_with("invalid ssh target"), "{err}");
        }
    }

    #[test]
    fn parses_effective_output() {
        let output = "user jacanales\nhostname pve00.jacd.local\nport 2222\nidentityfile ~/.ssh/id_ed25519\n";
        let target = parse_effective("pve00", output).expect("parses");
        assert_eq!(target.user, "jacanales");
        assert_eq!(target.hostname, "pve00.jacd.local");
        assert_eq!(target.port, 2222);
        assert_eq!(target.endpoint(), "jacanales@pve00.jacd.local:2222");
    }

    #[test]
    fn endpoint_hides_default_port() {
        let output = "user root\nhostname 10.0.0.5\nport 22\n";
        let target = parse_effective("node", output).expect("parses");
        assert_eq!(target.endpoint(), "root@10.0.0.5");
    }

    #[test]
    fn resolve_failure_surfaces_stderr() {
        let dir = std::env::temp_dir().join(format!("dse-ssh-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let stub = dir.join("ssh-fail");
        std::fs::write(&stub, "#!/bin/sh\necho 'no such host' >&2\nexit 255\n")
            .expect("writes stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let settings = SshSettings {
            bin: stub.display().to_string(),
            ..settings()
        };
        let err = resolve_target(&settings, "ghost").expect_err("fails");
        assert!(err.to_string().contains("no such host"), "{err}");
    }
}
