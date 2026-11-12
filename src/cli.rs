//! Command line surface (S1, S6): the TUI is the default, with scriptable
//! subcommands for the same engine.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc::unbounded_channel;

use crate::app::{App, Target};
use crate::config::{Config, ConfigError, TargetSpec};
use crate::exec::{HostState, RunEvent, RunSpec, RunSummary, Stream, spawn_run};
use crate::picker;
use crate::ssh::{self, SshSettings};
use crate::tui;

/// Default per-host limit for `dse ping`: long enough for a slow link, short
/// enough that a dead host does not stall the report.
const DEFAULT_PING_TIMEOUT_SECS: u64 = 10;

const AFTER_HELP: &str = r#"Hosts are plain ssh targets, so ~/.ssh/config, keys, agent and ProxyJump all apply.

Config: ~/.config/dse/config.toml (override with --config or $DSE_CONFIG)

  [defaults]
  concurrency = 16

  [groups]
  pve  = ["pve00.jacd.local", "pve01.jacd.local", "pve02.jacd.local"]
  prox = ["prox00.jacd.local"]
  all  = ["@pve", "@prox"]

Examples:
  dse init                     write a starter config.toml
  dse                          open the fan-out terminal on every group
  dse -g pve                   open the terminal on the pve group
  dse run -g pve apt update    run once and stream prefixed output
  dse hosts -g pve             show the effective ssh endpoint per host
  dse ping -g pve              check every host answers over ssh
  dse ping pve00.jacd.local    the same, for a single machine
  dse config [-e]              show the config file and its path (or edit it)
  dse list [-l]                groups and their host counts (and hosts)
  dse pick                     choose a group from a list, then open it
"#;

#[derive(Debug, Parser)]
#[command(name = "dse", version, about = "Fan one command out to a group of hosts over your existing SSH config", after_help = AFTER_HELP)]
struct Cli {
    /// Configured group to target (repeatable)
    #[arg(short = 'g', long = "group", value_name = "NAME", global = true)]
    group: Vec<String>,

    /// Explicit hosts, comma separated
    #[arg(
        short = 'H',
        long = "hosts",
        value_name = "HOST[,HOST...]",
        global = true
    )]
    hosts: Option<String>,

    /// Target every configured group
    #[arg(short = 'a', long = "all", global = true)]
    all: bool,

    /// Maximum concurrent ssh sessions
    #[arg(short = 'c', long = "concurrency", value_name = "N", global = true)]
    concurrency: Option<usize>,

    /// Path to config.toml
    #[arg(long = "config", value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check that hosts are reachable and answer over ssh
    Ping {
        /// Hosts or configured group names
        #[arg(value_name = "HOST|GROUP")]
        targets: Vec<String>,
        /// Per-host limit in seconds (0 waits forever)
        #[arg(long = "timeout", value_name = "SECS", default_value_t = DEFAULT_PING_TIMEOUT_SECS)]
        timeout: u64,
    },
    /// Write a starter config.toml and print its path
    Init {
        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
    },
    /// Open the fan-out terminal (default)
    Tui,
    /// Run one command and stream prefixed output to stdout
    Run {
        /// Command executed on every targeted host
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// List the configured groups
    #[command(visible_alias = "groups")]
    List {
        /// Also list every host of each group
        #[arg(short = 'l', long = "long")]
        long: bool,
    },
    /// Pick a group from a list, then open the terminal on it
    Pick,
    /// Show the config file and where it lives (or edit it)
    Config {
        /// Open the file in $VISUAL / $EDITOR
        #[arg(short = 'e', long = "edit")]
        edit: bool,
    },
    /// Resolve each targeted host through ssh -G
    Hosts,
}

/// Entry point used by `main`.
pub async fn run() -> ExitCode {
    match run_inner().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("dse: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner() -> Result<ExitCode> {
    let cli = Cli::parse();
    let command = cli.command.as_ref();

    // `init` must work before any config exists.
    if let Some(Command::Init { force }) = command {
        return run_init(&cli, *force);
    }
    // `config` is about the file itself, so it must not fail on a missing one.
    if let Some(Command::Config { edit }) = command {
        return run_config(&cli, *edit).await;
    }

    // Host listing needs no config file; everything else does.
    let (config, config_path) = match load_config(&cli) {
        Ok(loaded) => loaded,
        Err(err) => {
            if matches!(command, Some(Command::Hosts)) && cli.hosts.is_some() {
                (
                    Config::default(),
                    Config::path().unwrap_or_else(|_| PathBuf::from("<none>")),
                )
            } else {
                return Err(err);
            }
        }
    };
    let ssh = ssh_settings(&config);
    let concurrency = cli
        .concurrency
        .unwrap_or(config.defaults.concurrency)
        .max(1);

    match command {
        Some(Command::List { long }) => {
            for line in group_report(&config, &config_path, *long) {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Pick) => run_pick(&config, &cli, ssh, concurrency).await,
        Some(Command::Hosts) => {
            let hosts = resolve(&config, &cli)?;
            Ok(if list_hosts(&hosts, &ssh) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Some(Command::Run { command }) => {
            let hosts = resolve(&config, &cli)?;
            let spec = RunSpec {
                hosts,
                command: command.join(" "),
                concurrency,
                ssh,
                timeout: None,
            };
            run_once(spec).await
        }
        Some(Command::Ping { timeout, .. }) => {
            let hosts = ping_targets(&config, &cli)?;
            let spec = RunSpec {
                hosts,
                // Proving an ssh session works is the whole point: the lightest
                // possible remote command, no output, no side effects.
                command: "true".to_string(),
                concurrency,
                ssh,
                timeout: (*timeout > 0).then(|| std::time::Duration::from_secs(*timeout)),
            };
            ping(spec).await
        }
        Some(Command::Init { .. }) => unreachable!("init is handled before the config is loaded"),
        Some(Command::Config { .. }) => {
            unreachable!("config is handled before the config is loaded")
        }
        Some(Command::Tui) | None => {
            let targets = build_targets(&config, &cli)?;
            let app = App::new(targets, 0, ssh, concurrency);
            tui::run(app).await.context("terminal session failed")?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `dse pick`: choose a group from a list, then open the terminal on it.
async fn run_pick(
    config: &Config,
    cli: &Cli,
    ssh: SshSettings,
    concurrency: usize,
) -> Result<ExitCode> {
    let targets = build_targets(config, cli)?;
    if let Some(chosen) = picker::run(targets).await.context("group picker failed")? {
        let app = App::new(vec![chosen], 0, ssh, concurrency);
        tui::run(app).await.context("terminal session failed")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolve the config path, preferring an explicit `--config`.
fn config_path(cli: &Cli) -> Result<PathBuf> {
    match &cli.config {
        Some(path) => Ok(path.clone()),
        None => Config::path().context("cannot resolve the config path"),
    }
}

/// `dse config`: show the file and its location, or hand it to an editor.
async fn run_config(cli: &Cli, edit: bool) -> Result<ExitCode> {
    let path = config_path(cli)?;
    if edit {
        return edit_config(&path).await;
    }
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            for line in config_report(&path, &raw) {
                println!("{line}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("{}", path.display());
            println!("no config file yet: create one with `dse init`");
            Ok(ExitCode::FAILURE)
        }
        Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
    }
}

async fn edit_config(path: &std::path::Path) -> Result<ExitCode> {
    if !path.exists() {
        Config::write_template(path, false).context("cannot create config")?;
        println!("created {}", path.display());
    }
    let editor = editor_command(|key| std::env::var_os(key))
        .context("no editor configured: set $VISUAL or $EDITOR")?;
    let argv = editor_argv(&editor);
    let (bin, args) = argv.split_first().context("the editor command is empty")?;
    let status = tokio::process::Command::new(bin)
        .args(args)
        .arg(path)
        .status()
        .await
        .with_context(|| format!("cannot run {editor}"))?;
    if !status.success() {
        bail!("{editor} exited with {status}");
    }
    // Editing can break the file; say so now instead of at the next command.
    match Config::load_from(path) {
        Ok(config) => {
            println!(
                "{} · {} group{} · valid",
                path.display(),
                config.groups.len(),
                if config.groups.len() == 1 { "" } else { "s" }
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(err) => {
            eprintln!("dse: {} is not valid: {err}", path.display());
            Ok(ExitCode::FAILURE)
        }
    }
}

/// The file, preceded by its own path, so the output is self-describing.
fn config_report(path: &std::path::Path, raw: &str) -> Vec<String> {
    let mut lines = vec![path.display().to_string()];
    lines.extend(raw.lines().map(str::to_string));
    lines
}

/// `$VISUAL`, then `$EDITOR`, then `vi`. Blank values are ignored.
fn editor_command(lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<String> {
    for key in ["VISUAL", "EDITOR"] {
        if let Some(value) = lookup(key) {
            let value = value.to_string_lossy().trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    Some("vi".to_string())
}

/// Editors are configured as a command line (`code -w`), so split it.
fn editor_argv(editor: &str) -> Vec<String> {
    editor.split_whitespace().map(str::to_string).collect()
}

fn run_init(cli: &Cli, force: bool) -> Result<ExitCode> {
    let path = match &cli.config {
        Some(path) => path.clone(),
        None => Config::path().context("cannot resolve the config path")?,
    };
    let written = Config::write_template(&path, force).context("cannot write config")?;
    println!("wrote {}", written.display());
    println!("edit [groups] with your hosts, then run `dse`");
    Ok(ExitCode::SUCCESS)
}

/// Load the config and remember which file it came from, so reported paths are
/// always the ones actually used.
fn load_config(cli: &Cli) -> Result<(Config, PathBuf)> {
    match &cli.config {
        Some(path) => {
            let config = Config::load_from(path)
                .with_context(|| format!("cannot load config {}", path.display()))?;
            Ok((config, path.clone()))
        }
        None => match Config::load() {
            Ok((config, path)) => Ok((config, path)),
            Err(ConfigError::NotFound(path)) if cli.hosts.is_some() => {
                Ok((Config::default(), PathBuf::from(path)))
            }
            Err(err) => Err(err).context("cannot load config"),
        },
    }
}

fn ssh_settings(config: &Config) -> SshSettings {
    SshSettings {
        bin: config.defaults.ssh_bin.clone(),
        connect_timeout_secs: config.defaults.connect_timeout_secs.max(1),
        options: config.defaults.ssh_options.clone(),
    }
}

fn spec_for(cli: &Cli) -> Option<TargetSpec> {
    if cli.all {
        return Some(TargetSpec::All);
    }
    if let Some(hosts) = &cli.hosts {
        return Some(TargetSpec::Hosts(parse_host_list(hosts)));
    }
    if !cli.group.is_empty() {
        return Some(TargetSpec::Groups(cli.group.clone()));
    }
    None
}

fn resolve(config: &Config, cli: &Cli) -> Result<Vec<String>> {
    let Some(spec) = spec_for(cli) else {
        bail!("select a target with --group, --hosts or --all");
    };
    config.resolve(&spec).context("cannot resolve target")
}

/// Targets shown in the TUI: explicit selection, or every configured group.
fn build_targets(config: &Config, cli: &Cli) -> Result<Vec<Target>> {
    let mut targets = Vec::new();
    match spec_for(cli) {
        Some(spec) => {
            let hosts = config.resolve(&spec).context("cannot resolve target")?;
            targets.push(Target {
                label: label_for(cli),
                hosts,
            });
        }
        None => {
            for group in config.group_names() {
                let spec = TargetSpec::Groups(vec![group.clone()]);
                let hosts = config
                    .resolve(&spec)
                    .with_context(|| format!("group {group:?}"))?;
                targets.push(Target {
                    label: group,
                    hosts,
                });
            }
            if targets.is_empty() {
                bail!(
                    "no hosts selected: configure groups in ~/.config/dse/config.toml or pass --hosts"
                );
            }
        }
    }
    Ok(targets)
}

fn label_for(cli: &Cli) -> String {
    if cli.all {
        "all".to_string()
    } else if let Some(hosts) = &cli.hosts {
        format!("{} hosts", parse_host_list(hosts).len())
    } else if !cli.group.is_empty() {
        cli.group.join("+")
    } else {
        "all".to_string()
    }
}

fn parse_host_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
        .collect()
}

/// Render `dse groups` output; separated from stdout so it can be asserted.
fn group_report(config: &Config, path: &std::path::Path, long: bool) -> Vec<String> {
    let mut lines = vec![format!("config  {}", path.display())];
    if config.groups.is_empty() {
        lines.push("no groups defined".to_string());
        return lines;
    }
    for group in config.group_names() {
        match config.resolve(&TargetSpec::Groups(vec![group.clone()])) {
            Ok(hosts) => {
                lines.push(format!(
                    "{group:<12} {} host{}",
                    hosts.len(),
                    if hosts.len() == 1 { "" } else { "s" }
                ));
                if long {
                    lines.extend(hosts.iter().map(|host| format!("    {host}")));
                }
            }
            Err(err) => lines.push(format!("{group:<12} ! {err}")),
        }
    }
    lines
}

fn list_hosts(hosts: &[String], ssh: &SshSettings) -> bool {
    let mut ok = true;
    for host in hosts {
        match ssh::resolve_target(ssh, host) {
            Ok(target) => println!("{host:<28} {}", target.endpoint()),
            Err(err) => {
                ok = false;
                println!("{host:<28} ! {err}");
            }
        }
    }
    ok
}

async fn run_once(spec: RunSpec) -> Result<ExitCode> {
    let hosts = spec.hosts.clone();
    let width = hosts
        .iter()
        .map(|host| host.chars().count())
        .max()
        .unwrap_or(0);
    let color = use_color();
    let (tx, mut rx) = unbounded_channel();
    let handle = spawn_run(spec, tx);

    while let Some(event) = rx.recv().await {
        match event {
            RunEvent::Started { .. } => {}
            RunEvent::Output { idx, stream, line } => {
                let name = hosts.get(idx).map(String::as_str).unwrap_or("?");
                let tag = match stream {
                    Stream::Stdout => paint(color, "36", &format!("{name:<width$}")),
                    Stream::Stderr => paint(color, "31", &format!("{name:<width$}")),
                };
                println!("{tag} │ {line}");
            }
            RunEvent::Finished {
                idx,
                state,
                elapsed_ms,
            } => {
                if state.is_failure() {
                    let name = hosts.get(idx).map(String::as_str).unwrap_or("?");
                    println!(
                        "{} {name:<width$} {} · {:.1}s",
                        paint(color, "31", "✖"),
                        describe(&state),
                        elapsed_ms as f64 / 1000.0
                    );
                }
            }
        }
    }

    let summary = handle.wait().await;
    print_summary(&summary, color);
    Ok(ExitCode::from(summary.exit_code() as u8))
}

fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// One host of a `dse ping` report.
struct PingRow {
    host: String,
    state: HostState,
    elapsed_ms: u128,
}

/// `dse ping` accepts either explicit hosts or configured group names, so
/// `dse ping pve` and `dse ping pve00.jacd.local` both work.
fn ping_targets(config: &Config, cli: &Cli) -> Result<Vec<String>> {
    let Some(Command::Ping { targets, .. }) = &cli.command else {
        bail!("internal error: ping_targets called for another command");
    };
    if targets.is_empty() {
        if spec_for(cli).is_none() {
            bail!("select a host or group to ping, e.g. `dse ping pve` or `dse ping -g pve`");
        }
        return resolve(config, cli);
    }

    let mut hosts = Vec::new();
    for raw in targets {
        for name in parse_host_list(raw) {
            if config.groups.contains_key(&name) {
                let expanded = config
                    .resolve(&TargetSpec::Groups(vec![name.clone()]))
                    .with_context(|| format!("group {name:?}"))?;
                hosts.extend(expanded);
            } else {
                hosts.push(name);
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    hosts.retain(|host| seen.insert(host.clone()));
    if hosts.is_empty() {
        bail!("select a host or group to ping");
    }
    Ok(hosts)
}

/// Render the ping table; `false` means at least one host did not answer.
fn ping_report(rows: &[PingRow], color: bool) -> (Vec<String>, bool) {
    let host_width = rows
        .iter()
        .map(|row| row.host.chars().count())
        .max()
        .unwrap_or(0);
    let details: Vec<String> = rows
        .iter()
        .map(|row| match &row.state {
            HostState::Succeeded => format!("{} ms", row.elapsed_ms),
            other => describe(other),
        })
        .collect();
    let detail_width = details
        .iter()
        .map(|detail| detail.chars().count())
        .max()
        .unwrap_or(0);

    let mut all_reachable = true;
    let lines = rows
        .iter()
        .zip(details.iter())
        .map(|(row, detail)| {
            let (glyph, code) = if row.state == HostState::Succeeded {
                ("✔", "32")
            } else {
                all_reachable = false;
                ("✖", "31")
            };
            format!(
                "{} {:<host_width$} {:>detail_width$}",
                paint(color, code, glyph),
                row.host,
                detail
            )
        })
        .collect();
    (lines, all_reachable)
}

/// Probe every host with a trivial ssh command; latency is the ssh round trip.
async fn ping(spec: RunSpec) -> Result<ExitCode> {
    let hosts = spec.hosts.clone();
    let color = use_color();
    let (tx, mut rx) = unbounded_channel();
    let handle = spawn_run(spec, tx);

    let mut rows: Vec<Option<PingRow>> = hosts.iter().map(|_| None).collect();
    while let Some(event) = rx.recv().await {
        let RunEvent::Finished {
            idx,
            state,
            elapsed_ms,
        } = event
        else {
            continue;
        };
        let Some(slot) = rows.get_mut(idx) else {
            continue;
        };
        *slot = Some(PingRow {
            host: hosts[idx].clone(),
            state,
            elapsed_ms,
        });
    }
    let _summary = handle.wait().await;

    let rows: Vec<PingRow> = rows.into_iter().flatten().collect();
    let total = rows.len();
    let reachable = rows
        .iter()
        .filter(|row| row.state == HostState::Succeeded)
        .count();
    let (lines, all_reachable) = ping_report(&rows, color);
    for line in lines {
        println!("{line}");
    }
    if all_reachable {
        println!("{} {reachable}/{total} reachable", paint(color, "32", "✔"));
        Ok(ExitCode::SUCCESS)
    } else {
        println!(
            "{} {reachable}/{total} reachable · {} unreachable",
            paint(color, "31", "✖"),
            total - reachable
        );
        Ok(ExitCode::FAILURE)
    }
}

fn describe(state: &HostState) -> String {
    match state {
        HostState::Succeeded => "ok".to_string(),
        HostState::Failed(Some(code)) => format!("exit {code}"),
        HostState::Failed(None) => "signal".to_string(),
        HostState::Errored(message) => format!("error: {message}"),
        HostState::TimedOut(limit) => format!("timeout after {:.1}s", limit.as_secs_f64()),
        HostState::Cancelled => "cancelled".to_string(),
        HostState::Pending => "queued".to_string(),
        HostState::Running => "running".to_string(),
    }
}

fn print_summary(summary: &RunSummary, color: bool) {
    let seconds = summary.elapsed_ms as f64 / 1000.0;
    if summary.is_success() {
        println!(
            "{} {} hosts · {seconds:.1}s",
            paint(color, "32", "✔"),
            summary.succeeded
        );
    } else {
        println!(
            "{} {} ok · {} failed · {seconds:.1}s",
            paint(color, "31", "✖"),
            summary.succeeded,
            summary.failures.len()
        );
    }
}

fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("\u{1b}[{code}m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::parse(
            r#"
[defaults]
concurrency = 4

[groups]
pve = ["pve00.jacd.local", "pve01.jacd.local"]
prox = ["prox00.jacd.local"]
all = ["@pve", "@prox"]
"#,
        )
        .expect("parses")
    }

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("cli parses")
    }

    #[test]
    fn ping_accepts_hosts_and_group_names_positionally() {
        let config = config();
        let spec =
            ping_targets(&config, &cli(&["dse", "ping", "pve", "10.0.0.5"])).expect("resolves");
        assert_eq!(
            spec,
            vec!["pve00.jacd.local", "pve01.jacd.local", "10.0.0.5"],
            "a configured group name expands, anything else is a host"
        );

        let spec =
            ping_targets(&config, &cli(&["dse", "ping", "pve00.jacd.local"])).expect("resolves");
        assert_eq!(spec, vec!["pve00.jacd.local"]);

        let spec = ping_targets(&config, &cli(&["dse", "ping", "-g", "prox"])).expect("resolves");
        assert_eq!(
            spec,
            vec!["prox00.jacd.local"],
            "without positional targets the -g/-H flags apply"
        );

        assert!(ping_targets(&config, &cli(&["dse", "ping"])).is_err());
    }

    #[test]
    fn ping_report_marks_reachable_and_unreachable_hosts() {
        let rows = vec![
            PingRow {
                host: "pve00.jacd.local".into(),
                state: HostState::Succeeded,
                elapsed_ms: 42,
            },
            PingRow {
                host: "pve01.jacd.local".into(),
                state: HostState::TimedOut(std::time::Duration::from_secs(10)),
                elapsed_ms: 10_000,
            },
            PingRow {
                host: "pve02.jacd.local".into(),
                state: HostState::Failed(Some(255)),
                elapsed_ms: 1200,
            },
        ];
        let (lines, all_reachable) = ping_report(&rows, false);
        assert!(!all_reachable);
        assert!(
            lines[0].contains("pve00.jacd.local") && lines[0].contains("42 ms"),
            "{lines:?}"
        );
        assert!(lines[1].contains("timeout after 10.0s"), "{lines:?}");
        assert!(lines[2].contains("exit 255"), "{lines:?}");
        assert_eq!(lines.len(), 3, "one line per host, in host order");
    }

    #[test]
    fn ping_parses_targets_and_a_timeout() {
        let plain = cli(&["dse", "ping"]);
        match plain.command {
            Some(Command::Ping {
                ref targets,
                timeout,
            }) => {
                assert!(targets.is_empty());
                assert_eq!(timeout, DEFAULT_PING_TIMEOUT_SECS);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        let tuned = cli(&["dse", "ping", "-H", "pve00.jacd.local", "--timeout", "3"]);
        match tuned.command {
            Some(Command::Ping { timeout, .. }) => assert_eq!(timeout, 3),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn config_prints_the_path_then_the_file() {
        let raw = "[defaults]\nconcurrency = 4\n\n[groups]\npve = [\"a\"]\n";
        let lines = config_report(std::path::Path::new("/tmp/x/config.toml"), raw);
        assert_eq!(lines[0], "/tmp/x/config.toml");
        assert_eq!(lines[1], "[defaults]");
        assert!(
            lines.iter().any(|line| line.contains("pve = [\"a\"]")),
            "{lines:?}"
        );
    }

    #[test]
    fn editor_follows_visual_then_editor_then_vi() {
        let set = |values: Vec<(&'static str, &'static str)>| {
            move |key: &str| {
                values
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| std::ffi::OsString::from(*value))
            }
        };
        assert_eq!(
            editor_command(set(vec![("VISUAL", "code -w"), ("EDITOR", "nano")])).as_deref(),
            Some("code -w"),
            "VISUAL wins"
        );
        assert_eq!(
            editor_command(set(vec![("EDITOR", "nano")])).as_deref(),
            Some("nano")
        );
        assert_eq!(
            editor_command(set(vec![])).as_deref(),
            Some("vi"),
            "fallback"
        );
        assert_eq!(
            editor_command(set(vec![("EDITOR", "   ")])).as_deref(),
            Some("vi"),
            "a blank variable is ignored"
        );
    }

    #[test]
    fn editor_command_lines_split_into_argv() {
        assert_eq!(editor_argv("code -w"), vec!["code", "-w"]);
        assert_eq!(editor_argv("  nano "), vec!["nano"]);
        assert_eq!(
            editor_argv("sh -c 'echo hi'"),
            vec!["sh", "-c", "'echo", "hi'"]
        );
    }

    #[test]
    fn config_and_config_edit_parse() {
        assert!(matches!(
            cli(&["dse", "config"]).command,
            Some(Command::Config { edit: false })
        ));
        assert!(matches!(
            cli(&["dse", "config", "-e"]).command,
            Some(Command::Config { edit: true })
        ));
        assert!(matches!(
            cli(&["dse", "config", "--edit"]).command,
            Some(Command::Config { edit: true })
        ));
    }

    #[test]
    fn list_shows_groups_and_optionally_their_hosts() {
        let config = config();
        let short = group_report(&config, std::path::Path::new("x.toml"), false);
        assert!(
            short
                .iter()
                .any(|line| line.starts_with("pve ") && line.contains("2 hosts"))
        );
        assert!(
            !short.iter().any(|line| line.contains("pve00.jacd.local")),
            "{short:?}"
        );

        let long = group_report(&config, std::path::Path::new("x.toml"), true);
        assert!(
            long.iter().any(|line| line.trim() == "pve00.jacd.local"),
            "every host is listed with --long: {long:?}"
        );
        assert!(long.iter().any(|line| line.contains("hosts")), "{long:?}");
    }

    #[test]
    fn list_and_pick_parse_with_the_groups_alias() {
        assert!(matches!(
            cli(&["dse", "list"]).command,
            Some(Command::List { long: false })
        ));
        assert!(matches!(
            cli(&["dse", "list", "--long"]).command,
            Some(Command::List { long: true })
        ));
        assert!(
            matches!(
                cli(&["dse", "groups"]).command,
                Some(Command::List { long: false })
            ),
            "the old name keeps working"
        );
        assert!(matches!(cli(&["dse", "pick"]).command, Some(Command::Pick)));
    }

    #[test]
    fn group_report_names_the_file_actually_used() {
        let report = group_report(
            &config(),
            std::path::Path::new("/tmp/custom/config.toml"),
            false,
        );
        assert_eq!(report[0], "config  /tmp/custom/config.toml");
        assert!(
            report
                .iter()
                .any(|line| line.starts_with("pve ") && line.contains("2 hosts")),
            "{report:?}"
        );

        let broken = Config::parse("[groups]\nbad = [\"@missing\"]").expect("parses");
        let report = group_report(&broken, std::path::Path::new("x.toml"), false);
        assert!(
            report[1].contains('!'),
            "a broken group is reported inline: {report:?}"
        );
    }

    #[test]
    fn init_takes_an_optional_force_flag_and_reuses_config_path() {
        let plain = cli(&["dse", "init"]);
        assert!(matches!(
            plain.command,
            Some(Command::Init { force: false })
        ));
        assert!(plain.config.is_none());

        let forced = cli(&["dse", "init", "--force", "--config", ".demo/dse.toml"]);
        assert!(matches!(
            forced.command,
            Some(Command::Init { force: true })
        ));
        assert_eq!(
            forced.config.as_deref(),
            Some(std::path::Path::new(".demo/dse.toml"))
        );
    }

    #[test]
    fn host_lists_split_on_commas_and_trim() {
        assert_eq!(parse_host_list(" a, b ,, c "), vec!["a", "b", "c"]);
        assert!(parse_host_list("  , ").is_empty());
    }

    #[test]
    fn selection_precedence_is_all_then_hosts_then_groups() {
        assert_eq!(
            spec_for(&cli(&["dse", "-a", "-g", "pve"])),
            Some(TargetSpec::All)
        );
        assert_eq!(
            spec_for(&cli(&["dse", "-H", "10.0.0.1,10.0.0.2", "-g", "pve"])),
            Some(TargetSpec::Hosts(vec![
                "10.0.0.1".into(),
                "10.0.0.2".into()
            ]))
        );
        assert_eq!(
            spec_for(&cli(&["dse", "-g", "pve", "-g", "prox"])),
            Some(TargetSpec::Groups(vec!["pve".into(), "prox".into()]))
        );
        assert_eq!(spec_for(&cli(&["dse"])), None);
    }

    #[test]
    fn tui_targets_cover_every_group_by_default() {
        let config = config();
        let targets = build_targets(&config, &cli(&["dse"])).expect("targets");
        let labels: Vec<&str> = targets.iter().map(|target| target.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["all", "prox", "pve"],
            "groups are listed alphabetically"
        );
        assert_eq!(
            targets[0].hosts.len(),
            3,
            "the all target nests both groups"
        );
    }

    #[test]
    fn tui_targets_follow_an_explicit_selection() {
        let config = config();
        let targets = build_targets(&config, &cli(&["dse", "-g", "prox"])).expect("targets");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "prox");
        assert_eq!(targets[0].hosts, vec!["prox00.jacd.local"]);
    }

    #[test]
    fn unknown_group_is_an_error_for_the_tui() {
        let config = config();
        let err = build_targets(&config, &cli(&["dse", "-g", "nope"])).expect_err("fails");
        assert!(err.to_string().contains("cannot resolve target"), "{err:#}");
    }

    #[test]
    fn concurrency_falls_back_to_config() {
        let config = config();
        assert_eq!(
            cli(&["dse"])
                .concurrency
                .unwrap_or(config.defaults.concurrency)
                .max(1),
            4
        );
        assert_eq!(
            cli(&["dse", "-c", "0"])
                .concurrency
                .unwrap_or(config.defaults.concurrency)
                .max(1),
            1
        );
    }

    #[test]
    fn host_list_does_not_need_a_config_file() {
        let cli = cli(&["dse", "-H", "10.0.0.1"]);
        assert!(cli.hosts.is_some());
        assert_eq!(
            resolve(&Config::default(), &cli).expect("resolves"),
            vec!["10.0.0.1"]
        );
    }

    #[test]
    fn paint_only_colors_when_enabled() {
        assert_eq!(paint(true, "32", "ok"), "\u{1b}[32mok\u{1b}[0m");
        assert_eq!(paint(false, "32", "ok"), "ok");
    }
}
