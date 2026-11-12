# dsh

Fan one command out to a group of hosts — over the SSH config you already have.

dsh is a terminal multiplexing shell for fleets: pick a group of machines, type a
command in the embedded prompt, and watch every host report its own progress,
output and exit code. It borrows the ideas that made `pssh` and `pdsh` useful
(parallel execution, per-host output, exit-code summary) and adds a live,
modern terminal interface.

```text
╭ dsh  pve · 2 hosts ──────────────────────────────────────────────────────────────────────────╮
│████████████████████ 2/2  1 ok · 1 failed · 0 running · 0 queued    0.0s  ████████████████████│
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
╭ type a command and press Enter to fan it out ────────────────────────────────────────────────╮
│ ✔ pve00.jacd.local                 ok   1.4s  Reading package lists…                         │
│ ✖ pve01.jacd.local           exit 100   0.9s  E: Unable to lock                              │
│                                                                                              │
│                                                                                              │
│                                                                                              │
│                                                                                              │
│                                                                                              │
│                                                                                              │
│                                                                                              │
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
╭ command ─────────────────────────────────────────────────────────────────────────────────────╮
│❯ apt update                                                                                  │
╰──────────────────────────────────────────────────────────────────────────────────────────────╯
 Enter run · Tab group · ↑↓ host · Enter                                        host 1/2 · 0.0s 
```

## Why another one

- **No connection settings inside dsh.** Hosts are plain OpenSSH targets, so
  `~/.ssh/config`, keys, agent, `ProxyJump`, `Include` and `known_hosts` behave
  exactly as they already do in your shell. A group is just a list of host names.
- **Rust async fan-out.** Every host runs concurrently under a bounded
  semaphore, streams `stdout` and `stderr` line by line, and is killed when the
  run is cancelled.
- **Progress you can read at a glance.** Live gauge, per-host state
  (`queued`, spinner, `ok`, `exit N`, `error`), durations and the last output
  line; press Enter on a host to read its full output.

## Install

```sh
cargo build --release          # ./target/release/dsh
cargo install --path .         # ~/.cargo/bin/dsh
```

> Note: `~/.cargo/bin` usually precedes Homebrew, so installing here shadows the
> Debian `dsh` distributed by Homebrew (`/opt/homebrew/bin/dsh`). Rename the
> binary in `Cargo.toml` if both must coexist.

## Configure

`~/.config/dsh/config.toml` (override with `--config PATH` or `$DSH_CONFIG`):

```toml
[defaults]
concurrency = 16              # simultaneous ssh sessions
connect_timeout_secs = 5      # forwarded as -o ConnectTimeout
ssh_options = []              # extra -o options, e.g. ["ServerAliveInterval=15"]
ssh_bin = "ssh"               # client to use

[groups]
pve     = ["pve00.jacd.local", "pve01.jacd.local", "pve02.jacd.local"]
prox    = ["prox00.jacd.local"]
cluster = ["@pve", "@prox"]   # "@name" nests another group
```

That is the whole configuration: no passwords, no keys, no connection modes.
Key-based ssh must already work for those host names.

## Use

```sh
dsh                          # terminal on every configured group (Tab cycles)
dsh -g pve                   # terminal on one group
dsh run -g pve apt update    # one-shot run, prefixed streaming output
dsh run -H 10.0.0.9,10.0.0.10 uptime
dsh groups                   # groups and their host counts
dsh hosts -g pve             # effective user@host:port resolved through ssh -G
```

Exit code of `dsh run` is `0` only when every host succeeded, `1` otherwise.

### Keys in the terminal

| Key | Action |
| --- | --- |
| `Enter` | run the typed command (with empty input: open the selected host) |
| `Tab` | switch target/group |
| `↑` `↓` | select host (in detail view: scroll) |
| `Ctrl-P` `Ctrl-N` | previous/next command from history |
| `Ctrl-A` `Ctrl-E` | cursor to start/end of the line |
| `Ctrl-U` | clear the line |
| `Ctrl-C` | cancel the running command, else clear the line |
| `Esc` | leave the detail view |
| `q` `Ctrl-D` | quit (when the line is empty) |

## How it works

- `src/config.rs` resolves groups, nests `@group` references, de-duplicates hosts
  and rejects unsafe entries (empty names, leading `-`, whitespace).
- `src/ssh.rs` builds `ssh -o BatchMode=yes -o ConnectTimeout=N <target> <cmd>`
  and reads effective settings with `ssh -G` for display.
- `src/exec.rs` is the engine: one task per host behind a semaphore, `stdout` and
  `stderr` pumped line by line into a typed event channel, `kill_on_drop` so
  cancelling a run kills the ssh children, and an aggregated summary.
- `src/app.rs` holds the UI state machine (prompt editing, history, per-host
  views, cancellation) and `src/ui.rs` renders it; both are covered by tests,
  including a render test that asserts against a `TestBackend` screen.
- `src/cli.rs` wires clap, the TUI and the one-shot runner.

## Tests

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The engine, config, ssh layer and UI are tested against stubs, so the suite runs
without any reachable machine:

- a shell-script `ssh` stub answers `-G` and simulates success, failure and
  slow hosts;
- one test proves the concurrency bound is respected by observing peak overlap;
- one test drives the whole path "type a command → every host of the group ran
  it" through the real engine.

`cargo test -- --ignored --nocapture print_preview` dumps the rendered screen,
useful when changing the layout.

## Roadmap

- Native ssh multiplexing (`ControlMaster`) or an in-process client to cut
  connection latency on big groups.
- Per-host timeout and retry policies.
- `sudo` prompts, remote file copy, and ordered/rolling execution.
- Filters over groups (`tag:proxmox`, ssh-config wildcards) instead of flat lists.
- Persisted history and per-host paging in the detail view.
