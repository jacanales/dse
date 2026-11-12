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
use crate::ssh::{self, SshSettings};
use crate::tui;

const AFTER_HELP: &str = r#"Hosts are plain ssh targets, so ~/.ssh/config, keys, agent and ProxyJump all apply.

Config: ~/.config/dsh/config.toml (override with --config or $DSH_CONFIG)

  [defaults]
  concurrency = 16

  [groups]
  pve  = ["pve00.jacd.local", "pve01.jacd.local", "pve02.jacd.local"]
  prox = ["prox00.jacd.local"]
  all  = ["@pve", "@prox"]

Examples:
  dsh                          open the fan-out terminal on every group
  dsh -g pve                   open the terminal on the pve group
  dsh run -g pve apt update    run once and stream prefixed output
  dsh hosts -g pve             show the effective ssh endpoint per host
"#;

#[derive(Debug, Parser)]
#[command(name = "dsh", version, about = "Fan one command out to a group of hosts over your existing SSH config", after_help = AFTER_HELP)]
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
    /// Open the fan-out terminal (default)
    Tui,
    /// Run one command and stream prefixed output to stdout
    Run {
        /// Command executed on every targeted host
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// List configured groups
    Groups,
    /// Resolve each targeted host through ssh -G
    Hosts,
}

/// Entry point used by `main`.
pub async fn run() -> ExitCode {
    match run_inner().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("dsh: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_inner() -> Result<ExitCode> {
    let cli = Cli::parse();
    let command = cli.command.as_ref();

    // Host listing needs no config file; everything else does.
    let config = match load_config(&cli) {
        Ok(config) => config,
        Err(err) => {
            if matches!(command, Some(Command::Hosts)) && cli.hosts.is_some() {
                Config::default()
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
        Some(Command::Groups) => {
            list_groups(&config);
            Ok(ExitCode::SUCCESS)
        }
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
            };
            run_once(spec).await
        }
        Some(Command::Tui) | None => {
            let targets = build_targets(&config, &cli)?;
            let app = App::new(targets, 0, ssh, concurrency);
            tui::run(app).await.context("terminal session failed")?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn load_config(cli: &Cli) -> Result<Config> {
    match &cli.config {
        Some(path) => Config::load_from(path)
            .with_context(|| format!("cannot load config {}", path.display())),
        None => match Config::load() {
            Ok((config, _)) => Ok(config),
            Err(ConfigError::NotFound(_)) if cli.hosts.is_some() => Ok(Config::default()),
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
                    "no hosts selected: configure groups in ~/.config/dsh/config.toml or pass --hosts"
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

fn list_groups(config: &Config) {
    println!("config  {}", config_path_hint());
    if config.groups.is_empty() {
        println!("no groups defined");
        return;
    }
    for group in config.group_names() {
        match config.resolve(&TargetSpec::Groups(vec![group.clone()])) {
            Ok(hosts) => println!(
                "{group:<12} {} host{}",
                hosts.len(),
                if hosts.len() == 1 { "" } else { "s" }
            ),
            Err(err) => println!("{group:<12} ! {err}"),
        }
    }
}

fn config_path_hint() -> String {
    Config::path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
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
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
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

fn describe(state: &HostState) -> String {
    match state {
        HostState::Succeeded => "ok".to_string(),
        HostState::Failed(Some(code)) => format!("exit {code}"),
        HostState::Failed(None) => "signal".to_string(),
        HostState::Errored(message) => format!("error: {message}"),
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
    fn host_lists_split_on_commas_and_trim() {
        assert_eq!(parse_host_list(" a, b ,, c "), vec!["a", "b", "c"]);
        assert!(parse_host_list("  , ").is_empty());
    }

    #[test]
    fn selection_precedence_is_all_then_hosts_then_groups() {
        assert_eq!(
            spec_for(&cli(&["dsh", "-a", "-g", "pve"])),
            Some(TargetSpec::All)
        );
        assert_eq!(
            spec_for(&cli(&["dsh", "-H", "10.0.0.1,10.0.0.2", "-g", "pve"])),
            Some(TargetSpec::Hosts(vec![
                "10.0.0.1".into(),
                "10.0.0.2".into()
            ]))
        );
        assert_eq!(
            spec_for(&cli(&["dsh", "-g", "pve", "-g", "prox"])),
            Some(TargetSpec::Groups(vec!["pve".into(), "prox".into()]))
        );
        assert_eq!(spec_for(&cli(&["dsh"])), None);
    }

    #[test]
    fn tui_targets_cover_every_group_by_default() {
        let config = config();
        let targets = build_targets(&config, &cli(&["dsh"])).expect("targets");
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
        let targets = build_targets(&config, &cli(&["dsh", "-g", "prox"])).expect("targets");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].label, "prox");
        assert_eq!(targets[0].hosts, vec!["prox00.jacd.local"]);
    }

    #[test]
    fn unknown_group_is_an_error_for_the_tui() {
        let config = config();
        let err = build_targets(&config, &cli(&["dsh", "-g", "nope"])).expect_err("fails");
        assert!(err.to_string().contains("cannot resolve target"), "{err:#}");
    }

    #[test]
    fn concurrency_falls_back_to_config() {
        let config = config();
        assert_eq!(
            cli(&["dsh"])
                .concurrency
                .unwrap_or(config.defaults.concurrency)
                .max(1),
            4
        );
        assert_eq!(
            cli(&["dsh", "-c", "0"])
                .concurrency
                .unwrap_or(config.defaults.concurrency)
                .max(1),
            1
        );
    }

    #[test]
    fn host_list_does_not_need_a_config_file() {
        let cli = cli(&["dsh", "-H", "10.0.0.1"]);
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
