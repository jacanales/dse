# dse

Fan one command out to a group of hosts — over the SSH config you already have.

dse is a terminal multiplexing shell for fleets: pick a group of machines, type a
command in the embedded prompt, and watch every host report its own progress,
output and exit code. It borrows the ideas that made `pssh` and `pdsh` useful
(parallel execution, per-host output, exit-code summary) and adds a live,
modern terminal interface.

```text
╭ dse  pve · 2 hosts ──────────────────────────────────────────────────────────────────────────╮
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

- **No connection settings inside dse.** Hosts are plain OpenSSH targets, so
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
cargo build --release          # ./target/release/dse
cargo install --path .         # ~/.cargo/bin/dse
```

The name is **dse** — *Distributed Shell Executor* — so nothing collides with the
classic Debian `dsh` (still installed by Homebrew as `/opt/homebrew/bin/dsh`).

## Configure

```sh
dse init        # writes a starter ~/.config/dse/config.toml (refuses to overwrite: --force)
```

`~/.config/dse/config.toml` is resolved as `$DSE_CONFIG`, else
`$XDG_CONFIG_HOME/dse/config.toml`, else `$HOME/.config/dse/config.toml`
(override per run with `--config PATH`). If only the pre-rename file exists
(`$DSH_CONFIG`, `~/.config/dsh/config.toml`), it is used as a fallback and
`dse groups` shows which file was read:

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
dse init                     # write a starter config and print its path
dse                          # terminal on every configured group (Tab cycles)
dse -g pve                   # terminal on one group
dse run -g pve apt update    # one-shot run, prefixed streaming output
dse run -H 10.0.0.9,10.0.0.10 uptime
dse groups                   # groups and their host counts
dse hosts -g pve             # effective user@host:port resolved through ssh -G
dse ping -g pve              # does every host answer over ssh?
dse ping pve00.jacd.local    # or a single machine
dse ping pve dead01          # hosts and group names can be mixed
```

Exit codes of `dse run` and `dse ping` are `0` only when every host succeeded
(or answered), `1` otherwise.

### Checking a fleet: `dse ping`

```text
✔ pve00.jacd.local 613 ms
✔ pve01.jacd.local 466 ms
✖ pve02.jacd.local exit 255
✖ dead01.jacd.local timeout after 1.0s
✖ 2/4 reachable · 2 unreachable
```

A probe is a full ssh session running `true`, so it validates what dse actually
needs: DNS, TCP, host key, key/agent authentication and the remote shell. The
latency is the whole round trip. `--timeout SECS` (default 10, `0` waits forever)
bounds a dead host; on expiry the ssh child is killed and reported as a timeout.
Nothing changes on the remote machines.

### Keys in the terminal

| Key | Action |
| --- | --- |
| `Enter` | run the typed command (with empty input: open the selected host) |
| `↑` `↓` | walk the command history |
| `Ctrl-R` | reverse-search the history, zsh style |
| `Ctrl-P` `Ctrl-N` | select previous/next host |
| `Ctrl-A` `Ctrl-E` | cursor to start/end of the line |
| `Ctrl-U` | clear the line |
| `Tab` | switch target/group |
| `Ctrl-C` | cancel the running command, else clear the line |
| `Esc` | leave the search or the detail view |
| `q` `Ctrl-D` | quit (when the line is empty) |

Inside the reverse search: type to filter (case-insensitive substring), `Ctrl-R`
or `↑` for older matches, `↓` or `Ctrl-S` for newer ones, `Enter` puts the match
in the prompt (it never runs it for you), `Esc` cancels and restores the line
you were typing.

### Command history

`dse` keeps the last 50 commands of the session, merging consecutive repeats
(zsh's `HIST_IGNORE_DUPS`): running `apt update` three times leaves one entry.
Walking with `↑` and back down restores the unfinished line you had typed.
History lives in memory for the session; it is not written to disk.

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
- Per-host timeout and retry policies for `dse run` and the TUI (the engine
  already bounds each host; only `ping` exposes it today).
- `sudo` prompts, remote file copy, and ordered/rolling execution.
- Filters over groups (`tag:proxmox`, ssh-config wildcards) instead of flat lists.
- Persisted history and per-host paging in the detail view.
