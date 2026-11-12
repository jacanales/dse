//! Fan-out engine: run one command on many hosts, concurrently and bounded (S2, S4, S7).
//!
//! The engine is UI-agnostic: it emits a typed event per host transition and
//! per output line through an unbounded channel, so the TUI, a plain CLI run
//! and the tests all consume the same stream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Semaphore, mpsc::UnboundedSender};
use tokio::task::{JoinHandle, JoinSet};

use crate::ssh::{SshError, SshInvocation, SshSettings};

/// Which remote stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// Terminal state of one host for the current run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    Pending,
    Running,
    Succeeded,
    /// The remote command exited with this code (None when killed by a signal).
    Failed(Option<i32>),
    /// dsh could not even start or finish the ssh process.
    Errored(String),
    /// The host did not answer within the configured timeout; the child was killed.
    TimedOut(Duration),
    /// The run was aborted before this host finished.
    Cancelled,
}

impl HostState {
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            HostState::Failed(_)
                | HostState::Errored(_)
                | HostState::TimedOut(_)
                | HostState::Cancelled
        )
    }
}

/// Everything the UI needs to render progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    Started {
        idx: usize,
    },
    Output {
        idx: usize,
        stream: Stream,
        line: String,
    },
    Finished {
        idx: usize,
        state: HostState,
        elapsed_ms: u128,
    },
}

/// One fan-out request.
#[derive(Debug, Clone)]
pub struct RunSpec {
    pub hosts: Vec<String>,
    pub command: String,
    pub concurrency: usize,
    pub ssh: SshSettings,
    /// Per-host limit; `None` waits as long as ssh needs. On expiry the child is
    /// killed and the host is reported as `TimedOut`.
    pub timeout: Option<Duration>,
}

/// How the run ended, aggregated across hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub total: usize,
    pub succeeded: usize,
    pub failures: Vec<Failure>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub idx: usize,
    pub host: String,
    pub state: HostState,
}

impl RunSummary {
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }

    /// Process exit code: 0 only when every host succeeded.
    pub fn exit_code(&self) -> i32 {
        if self.is_success() { 0 } else { 1 }
    }
}

/// Handle to a running fan-out; abort the task to cancel every child.
pub struct RunHandle {
    task: JoinHandle<RunSummary>,
}

impl RunHandle {
    pub async fn wait(self) -> RunSummary {
        self.task.await.unwrap_or(RunSummary {
            total: 0,
            succeeded: 0,
            failures: vec![Failure {
                idx: 0,
                host: "<run>".to_string(),
                state: HostState::Errored("fan-out task panicked".to_string()),
            }],
            elapsed_ms: 0,
        })
    }

    /// Cancel the whole run. Children are spawned with `kill_on_drop`, so
    /// aborting the task kills every in-flight ssh process.
    pub fn cancel(&self) {
        self.task.abort();
    }
}

/// Start a fan-out run. Events are delivered in arrival order; the returned
/// handle yields the aggregated summary.
pub fn spawn_run(spec: RunSpec, events: UnboundedSender<RunEvent>) -> RunHandle {
    let task = tokio::spawn(async move {
        let started = Instant::now();
        let hosts = spec.hosts;
        let total = hosts.len();
        let permits = spec.concurrency.max(1);
        let gate = Arc::new(Semaphore::new(permits));
        let mut tasks: JoinSet<(usize, HostState)> = JoinSet::new();

        let timeout = spec.timeout;
        for (idx, host) in hosts.iter().cloned().enumerate() {
            let gate = Arc::clone(&gate);
            let events = events.clone();
            let command = spec.command.clone();
            let ssh = spec.ssh.clone();
            tasks.spawn(async move {
                let Ok(_permit) = gate.acquire_owned().await else {
                    return (idx, HostState::Cancelled);
                };
                let state = run_host(idx, &host, &command, &ssh, timeout, &events).await;
                (idx, state)
            });
        }
        drop(events);

        let mut states: Vec<Option<HostState>> = vec![None; total];
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((idx, state)) => {
                    if let Some(slot) = states.get_mut(idx) {
                        *slot = Some(state);
                    }
                }
                Err(err) => {
                    // A panicking or aborted host task must not stall the run.
                    let _ = err;
                }
            }
        }

        let mut succeeded = 0;
        let mut failures = Vec::new();
        for (idx, state) in states.into_iter().enumerate() {
            match state.unwrap_or(HostState::Cancelled) {
                HostState::Succeeded => succeeded += 1,
                other => failures.push(Failure {
                    idx,
                    host: hosts[idx].clone(),
                    state: other,
                }),
            }
        }

        RunSummary {
            total,
            succeeded,
            failures,
            elapsed_ms: started.elapsed().as_millis(),
        }
    });

    RunHandle { task }
}

async fn run_host(
    idx: usize,
    host: &str,
    command: &str,
    ssh: &SshSettings,
    limit: Option<Duration>,
    events: &UnboundedSender<RunEvent>,
) -> HostState {
    let _ = events.send(RunEvent::Started { idx });
    let started = Instant::now();

    let invocation = match SshInvocation::remote(ssh, host, command) {
        Ok(invocation) => invocation,
        Err(err) => return finish(idx, events, started, errored(err)),
    };

    let spawned = Command::new(&invocation.bin)
        .args(&invocation.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            return finish(
                idx,
                events,
                started,
                errored(SshError::Spawn {
                    bin: invocation.bin,
                    source: err,
                }),
            );
        }
    };

    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return finish(
            idx,
            events,
            started,
            errored_str("ssh streams were not piped"),
        );
    };

    let out_pump = tokio::spawn(pump(stdout, events.clone(), idx, Stream::Stdout));
    let err_pump = tokio::spawn(pump(stderr, events.clone(), idx, Stream::Stderr));
    let waited = wait_bounded(&mut child, limit).await;

    let state = match waited {
        // A timed-out host is abandoned: its pipes may still be held by a
        // grandchild that outlives the kill, and waiting for EOF would defeat
        // the timeout.
        Wait::TimedOut(limit) => {
            out_pump.abort();
            err_pump.abort();
            HostState::TimedOut(limit)
        }
        Wait::Exited(status) => {
            // Draining before finishing keeps the last lines of output.
            let _ = out_pump.await;
            let _ = err_pump.await;
            match status {
                Ok(status) if status.success() => HostState::Succeeded,
                Ok(status) => HostState::Failed(status.code()),
                Err(err) => errored_str(&err.to_string()),
            }
        }
    };
    finish(idx, events, started, state)
}

enum Wait {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut(Duration),
}

/// Wait for the child, killing it when the per-host limit expires so the output
/// pumps also end and the run does not hang on a dead host.
async fn wait_bounded(child: &mut Child, limit: Option<Duration>) -> Wait {
    let Some(limit) = limit else {
        return Wait::Exited(child.wait().await);
    };
    match tokio::time::timeout(limit, child.wait()).await {
        Ok(status) => Wait::Exited(status),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Wait::TimedOut(limit)
        }
    }
}

/// Forward every line of one remote stream as its own event.
async fn pump<R>(reader: R, events: UnboundedSender<RunEvent>, idx: usize, stream: Stream)
where
    R: AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if events.send(RunEvent::Output { idx, stream, line }).is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(_) => return,
        }
    }
}

fn finish(
    idx: usize,
    events: &UnboundedSender<RunEvent>,
    started: Instant,
    state: HostState,
) -> HostState {
    let _ = events.send(RunEvent::Finished {
        idx,
        state: state.clone(),
        elapsed_ms: started.elapsed().as_millis(),
    });
    state
}

fn errored(err: SshError) -> HostState {
    HostState::Errored(err.to_string())
}

fn errored_str(message: &str) -> HostState {
    HostState::Errored(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::sync::mpsc::unbounded_channel;

    /// A stub ssh: args are `-o BatchMode=yes -o ConnectTimeout=N <host> <command>`,
    /// so the target is $5 and the remote command is $6.
    fn stub(dir: &Path, body: &str) -> SshSettings {
        let path = dir.join("ssh");
        std::fs::write(
            &path,
            format!("#!/bin/sh\nDIR=\"{}\";\n{body}", dir.display()),
        )
        .expect("stub written");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        SshSettings {
            bin: path.display().to_string(),
            connect_timeout_secs: 1,
            options: Vec::new(),
        }
    }

    fn spec(ssh: SshSettings, hosts: &[&str], command: &str, concurrency: usize) -> RunSpec {
        RunSpec {
            hosts: hosts.iter().map(|h| h.to_string()).collect(),
            command: command.to_string(),
            concurrency,
            ssh,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn a_host_that_never_answers_times_out_and_stops_early() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ssh = stub(dir.path(), "sleep 5\nexit 0\n");
        let mut run = spec(ssh, &["slow"], "true", 2);
        run.timeout = Some(std::time::Duration::from_millis(300));

        let started = std::time::Instant::now();
        let (tx, mut rx) = unbounded_channel();
        let handle = spawn_run(run, tx);
        while rx.recv().await.is_some() {}
        let summary = handle.wait().await;
        let elapsed = started.elapsed();

        assert_eq!(summary.succeeded, 0);
        let expected = HostState::TimedOut(std::time::Duration::from_millis(300));
        assert_eq!(summary.failures[0].state, expected);
        assert_eq!(summary.exit_code(), 1);
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "the probe must not wait for the child: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_fast_host_still_succeeds_under_a_timeout() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ssh = stub(dir.path(), "echo quick\nexit 0\n");
        let mut run = spec(ssh, &["fast"], "true", 1);
        run.timeout = Some(std::time::Duration::from_secs(10));
        let (tx, mut rx) = unbounded_channel();
        let handle = spawn_run(run, tx);
        while rx.recv().await.is_some() {}
        let summary = handle.wait().await;
        assert_eq!(summary.succeeded, 1, "{summary:?}");
    }

    #[tokio::test]
    async fn reports_success_failure_and_forwarded_output() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ssh = stub(
            dir.path(),
            "host=\"$5\"; cmd=\"$6\"\ncase \"$host\" in\n  ok*) echo \"ran [$cmd] on $host\"; exit 0;;\n  *) echo \"boom $host\" >&2; exit 3;;\nesac\n",
        );
        let (tx, mut rx) = unbounded_channel();
        let handle = spawn_run(spec(ssh, &["ok1", "bad1"], "apt update", 4), tx);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        let summary = handle.wait().await;

        assert_eq!(summary.total, 2);
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failures.len(), 1);
        assert_eq!(summary.failures[0].host, "bad1");
        assert_eq!(summary.failures[0].state, HostState::Failed(Some(3)));
        assert_eq!(summary.exit_code(), 1);

        let stdout_line = events
            .iter()
            .find_map(|event| match event {
                RunEvent::Output {
                    stream: Stream::Stdout,
                    line,
                    ..
                } => Some(line.clone()),
                _ => None,
            })
            .expect("stdout line");
        assert_eq!(stdout_line, "ran [apt update] on ok1");

        let stderr_line = events
            .iter()
            .find_map(|event| match event {
                RunEvent::Output {
                    stream: Stream::Stderr,
                    line,
                    ..
                } => Some(line.clone()),
                _ => None,
            })
            .expect("stderr line");
        assert_eq!(stderr_line, "boom bad1");
    }

    #[tokio::test]
    async fn all_success_exits_zero() {
        let dir = tempfile::tempdir().expect("temp dir");
        let ssh = stub(dir.path(), "echo ok\nexit 0\n");
        let (tx, mut rx) = unbounded_channel();
        let handle = spawn_run(spec(ssh, &["a", "b", "c"], "true", 2), tx);
        while rx.recv().await.is_some() {}
        let summary = handle.wait().await;
        assert_eq!(summary.succeeded, 3);
        assert!(summary.is_success());
        assert_eq!(summary.exit_code(), 0);
    }

    #[tokio::test]
    async fn concurrency_is_bounded_by_the_spec() {
        let dir = tempfile::tempdir().expect("temp dir");
        let body = r#"lock() { while ! mkdir "$DIR/lock" 2>/dev/null; do sleep 0.005; done; }
unlock() { rmdir "$DIR/lock"; }
lock
n=$(cat "$DIR/live" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$DIR/live"
max=$(cat "$DIR/max" 2>/dev/null || echo 0)
if [ "$n" -gt "$max" ]; then echo "$n" > "$DIR/max"; fi
unlock
sleep 0.15
lock
n=$(cat "$DIR/live")
echo "$((n - 1))" > "$DIR/live"
unlock
exit 0
"#;
        let ssh = stub(dir.path(), body);
        let (tx, mut rx) = unbounded_channel();
        let handle = spawn_run(spec(ssh, &["a", "b", "c", "d"], "true", 2), tx);
        while rx.recv().await.is_some() {}
        let summary = handle.wait().await;

        assert_eq!(summary.succeeded, 4);
        let peak: i32 = std::fs::read_to_string(dir.path().join("max"))
            .expect("max recorded")
            .trim()
            .parse()
            .expect("number");
        assert_eq!(peak, 2, "peak concurrency must equal the configured bound");
    }

    #[tokio::test]
    async fn missing_binary_reports_a_host_error() {
        let settings = SshSettings {
            bin: "/nonexistent/ssh".to_string(),
            connect_timeout_secs: 1,
            options: Vec::new(),
        };
        let (tx, mut rx) = unbounded_channel();
        let handle = spawn_run(spec(settings, &["a"], "true", 1), tx);
        while rx.recv().await.is_some() {}
        let summary = handle.wait().await;
        assert_eq!(summary.succeeded, 0);
        assert!(matches!(summary.failures[0].state, HostState::Errored(_)));
        assert_eq!(summary.exit_code(), 1);
    }
}
