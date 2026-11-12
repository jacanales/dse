# dse — fan-out shell PoC

Rust TUI that fans one command out to a group of hosts, borrowing connection settings from the user's existing OpenSSH configuration.
PoC scope: runnable end-to-end (config, fan-out engine, live TUI), not polished.

> Naming: the project shipped as `dsh` and was renamed to `dse` (Distributed Shell
> Executor) in L15. Earlier log entries keep the old name verbatim as history.

## Specs

- **S1** — Terminal application, similar to dsh, to manage and send commands to several machines, machine groups or clusters at once. Quote: "Quiero tener una aplicación de terminal parecida a dsh para poder gestionar y enviar comandos a varias máquinas, grupos de máquinas o clusters de una sola vez."
- **S2** — An embedded sub-terminal (prompt inside the app): typing a command and pressing Enter runs it on every host of the selected group. Quote: "que me abra una subterminal, escribir "apt update", y ejecute ese comando en todas las máquinas del grupo de máquinas que haya elegido".
- **S3** — Live, pretty, modern and elegant progress per host while a command runs, showing failures and errors. Quote: "cuando lance un comando, quiero ver el progreso, de una forma bonita, moderna y elegante, si falla, y si hay error".
- **S4** — Rust, using its strengths (async fan-out, no shell glue). Quote: "Quiero que uses rust y la potencia de rust para hacer esto."
- **S5** — No connection settings inside the app: hosts are plain OpenSSH targets resolved from the user's SSH config (key auth already working). A group is just a list of hosts, e.g. `pve00.jacd.local, pve01.jacd.local, pve02.jacd.local`. Quotes: "en lugar de configurar el modo de conexión en la propia app, aprovecharemos la configuración de ssh", "para crear un grupo de máquinas simplemente tendremos que configurar: "pve00.jacd.local, pve01.jacd.local, pve02.jacd.local" y como ya tengo SSH configurado para conectar por clave a esas máquinas, podrá conectarse".
- **S6** — First deliverable is an initial PoC, explicitly a base to keep polishing into a fast, modern, easy tool. Quote: "Con esta información crea una PoC inicial y luego la iremos puliendo para tener una herramienta facil de utilizar, rápida y moderna."
- **S10** — The in-app prompt must keep a command history that can be navigated with the up and down arrows, like a terminal. Quote: "Además de poder navegar por el historial usando las flechas de arriba y abajo."
- **S11** — `Ctrl-R` must search that history and let the found command be reused, like zsh. Quote: "poder reusar el histórico de comandos, similar a cuando ejecutamos ctrl+r en zsh que podemos buscar un comando en el historial y volver a utilizarlo".
- **S8** — A command that pings a machine or a group to validate reachability before running anything. Quote: "Ahora quiero añadir un comando para enviar fácilmente un ping a una máquina o un grupo de máquinas, que nos servirá para validar si podemos conectar con esas máquinas y/o están activas".
- **S9** — The probe must terminate: an inactive host may not stall the report. Implementation: `--timeout SECS` (default 10, `0` = unlimited); on expiry the ssh child is killed and the host is reported as a timeout. Derived from S8 ("están activas").
- **S7** — Borrow feature ideas from pssh/pdsh (parallel execution, per-host output labeling, exit-code summary, non-interactive auth). Quote: "Algunos proyectos similares que hacen esto y de los que puedes coger ideas son: pssh, pdsh."

### PoC decisions (S5, S6)

- Connection layer = the system `ssh` client, so `~/.ssh/config`, keys, agent, `ProxyJump`, `KnownHosts` all apply with zero reimplementation. Effective settings are read back with `ssh -G <target>` for display only.
- Groups live in `~/.config/dse/config.toml` (`[groups]`), one flat list per group; `@group` entries nest groups. No host credentials in that file. The pre-rename path (`~/.config/dsh/config.toml`, `$DSH_CONFIG`) is read as a fallback while it is the only file that exists.
- Binary and crate name `dse` — Distributed Shell Executor (L15). It no longer collides with the Debian `dsh` installed by Homebrew.

## Tasks

- [x] **T1** — *S5* — Config + target resolution + SSH resolution (`src/config.rs`, `src/ssh.rs`), with tests. Commit: `f15b251`.
- [x] **T2** — *S2, S4, S7* — Fan-out engine: concurrent bounded execution, per-host stdout/stderr/exit events, summary + exit code (`src/exec.rs`), tests against a stub ssh. Commit: `f15b251`.
- [x] **T3** — *S3* — TUI: prompt line, per-host live status, progress gauge, detail view (`src/app.rs`, `src/ui.rs`, `src/tui.rs`). Commit: `f15b251`.
- [x] **T4** — *S1, S2, S6* — CLI wiring: `dse` (TUI), `dse run`, `dse groups`, `dse hosts` (`src/main.rs`, `src/cli.rs`). Commit: `f15b251`.
- [x] **T5** — *S6* — README with config example and keybindings; `cargo fmt`, `cargo clippy -D warnings`, `cargo test`. Commit: `f86fd8c`.

- [x] **T8** — *S10, S11* (user request L12) — terminal-style history: `↑`/`↓`
  walk it (restoring the unfinished line), `Ctrl-R` opens a zsh-style reverse
  search over it; the prompt editor gains a `Search` focus, host selection moves
  to `Ctrl-P`/`Ctrl-N`. Commit: `feat(tui): zsh-style command history`.
- [x] **T7** — *S8, S9* (user request L10) — `dse ping`: a bounded ssh probe per
  host (hosts, groups or a mix), latency per host, reason on failure. Engine
  gains an optional per-host timeout and `HostState::TimedOut`. Commit:
  `7382c99`.
- [x] **T6** — *S6* (user report L7) — `dse init` writes a starter config and the
  "config not found" error points at it; `dse groups` reports the file actually
  used. Commit: `c239cbc`.
- [x] **T9** — *S12* (user request L15) — rename to `dse`: crate, binary, config
  path `~/.config/dse/config.toml`, `$DSE_CONFIG`, docs, hints and messages, with
  the pre-rename config path kept as a read-only fallback. Commit:
  `refactor: rename dsh to dse`.

T1–T4 landed in one commit: a greenfield binary needs every module declared in
`main.rs` to compile, so slicing per task would have produced non-building
commits. Review workload risk: ~2.3k authored lines in `f15b251`; the next
increments land as separate work units.

## Log

- **L1** (user, verbatim):

```text
Quiero tener una aplicación de terminal parecida a dsh para poder gestionar y enviar comandos a varias máquinas, grupos de máquinas o clusters de una sola vez. Por ejemplo, que me abra una subterminal, escribir "apt update", y ejecute ese comando en todas las máquinas del grupo de máquinas que haya elegido. Además cuando lance un comando, quiero ver el progreso, de una forma bonita, moderna y elegante, si falla, y si hay error.

Quiero que uses rust y la potencia de rust para hacer esto.

Algunos proyectos similares que hacen esto y de los que puedes coger ideas son: pssh, pdsh.

Para conectar a las máquinas, en lugar de configurar el modo de conexión en la propia app, aprovecharemos la configuración de ssh. Es decir, para crear un grupo de máquinas simplemente tendremos que configurar: "pve00.jacd.local, pve01.jacd.local, pve02.jacd.local" y como ya tengo SSH configurado para conectar por clave a esas máquinas, podrá conectarse. Con esta información crea una PoC inicial y luego la iremos puliendo para tener una herramienta facil de utilizar, rápida y moderna.
```

- **L2** (decision, S5) — Connection layer is the system `ssh` binary, not a
  native client: `~/.ssh/config`, keys, agent and `ProxyJump` keep working with
  zero reimplementation. Fresh `ssh -G <target>` is used only to display the
  effective `user@host:port`. A native/multiplexed client stays on the roadmap.
- **L3** (decision, S2) — The "sub-terminal" is an in-TUI prompt, not a child
  terminal: the same engine feeds both the TUI and `dsh run`, so behavior and
  exit codes cannot drift between interactive and scripted use.
- **L4** (decision, S5) — Groups live in `~/.config/dsh/config.toml`; nesting via
  `@group`. The file holds host names only, never credentials.
- **L5** (evidence) — `cargo test`: 39 passed, 1 ignored (screen dump aid);
  `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
  End-to-end CLI demo against a scripted ssh stub: `dsh groups`, `dsh hosts -g pve`,
  `dsh run -g pve apt update` (exit 1 with one stub host failing 100), `dsh run -H ...`.
  Interactive TUI smoke-tested through a pseudo-terminal (alternate screen
  entered and restored, quit on `q`); visual proof comes from the TestBackend
  render test and the preview in README.
- **L7** (user report) — "It seems that the config.toml file is not found: dsh:
  cannot load config: no config file found; looked for
  /Users/jacanales/.config/dsh/config.toml". Cause: the file did not exist yet
  (neither `$DSH_CONFIG` nor `$XDG_CONFIG_HOME` is set). Fix: `dsh init` plus an
  error message naming it. The user then created the file by hand and it loads.
- **L8** (bug found while fixing L7) — `dsh groups --config <path>` printed the
  default path instead of the one used. Fixed: the loader returns the resolved
  path and `group_report` renders it; pinned by
  `group_report_names_the_file_actually_used`.
- **L9** (evidence, real config) — With the user's `~/.config/dsh/config.toml`:
  `dsh groups` → all 4 / prox 1 / pve 3; `dsh hosts -g pve` resolved every host
  through `ssh -G` to `jacanales@pveNN.jacd.local`; `dsh run -g prox uptime`
  failed fast because `prox00.jacd.local` does not resolve in DNS
  (`exit 255`, reported per host, dsh exit code 1). `prox` came from the README
  example, not from the user's fleet.
- **L10** (user request, verbatim) — "Ahora quiero añadir un comando para enviar
  fácilmente un ping a una máquina o un grupo de máquinas, que nos servirá para
  validar si podemos conectar con esas máquinas y/o están activas".
- **L11** (evidence, S8/S9) — `dsh ping -g pve` against the real fleet: 5/5
  reachable (391–613 ms) and `dsh ping pve00.jacd.local`: 1/1 (187 ms). Stub
  checks: a host exiting 100 → "exit 100"; a host that never answers →
  "timeout after 1.0s" and the child is killed, the suite still runs in ~0.6 s.
  Differential check: `dsh run` output and the rendered TUI screen are unchanged
  by the engine timeout addition; 49 tests, clippy and fmt clean.
- **L12** (user request, verbatim) — "Una mejora que quiero implementar ahora, es
  poder reusar el histórico de comandos, similar a cuando ejecutamos ctrl+r en
  zsh que podemos buscar un comando en el historial y volver a utilizarlo.
  Además de poder navegar por el historial usando las flechas de arriba y abajo.
  Quiero replicar estas funcionalidades del historial de la terminal en este
  programa."
- **L13** (evidence, S10/S11) — Unit tests cover the keymap and the search state
  machine (56 tests, plus the TUI keymap tests: `Ctrl-R` opens the search, `↑↓`
  walk history, `Ctrl-P/N` move the host selection, `Esc` restores the line).
  Real-terminal proof: a pty driver (`.demo/drive_tui.py`, scratch) types a
  command, presses `Ctrl-R`, filters "upt" and confirms the app painted
  `(reverse-i-search)` with the matched command, and that `Enter` left the
  search. Behaviour change to note: `↑`/`↓` no longer select hosts, and
  `Ctrl-P`/`Ctrl-N` do; hints and README updated.
- **Open question (L14)** — history lives in memory for the session only.
  Persisting it (`HISTFILE`-like) writes fleet commands to disk, so it is the
  user's call and not implemented yet.
- **L6** (closed by L11) — Real-machine verification done: `dsh ping` proves ssh
  connectivity to the whole `pve` group. A successful `dsh run` on the group is
  still the user's own next step.
- **L15** (user request, verbatim) — "No quiero llamar a esta app "dsh", porque se
  puede confundir con la dsh original. Me puedes proponer nombres cortos, pero
  consentido?" Follow-ups asked for three-letter names and then for initialisms of
  three words; the user chose **dse** (Distributed Shell Executor) from
  `dse` / `mos` / `res` / `fos`. Every candidate was checked as free in PATH (and
  the earlier shortlist in crates.io).
- **L16** (evidence, S12) — Rename applied: crate/binary `dse`, config path
  `~/.config/dse/config.toml`, `$DSE_CONFIG`, all help/hints/messages/docs. The
  pre-rename config (`$DSH_CONFIG`, `~/.config/dsh/config.toml`) is still read when
  the new path is absent (pinned by `legacy_paths_keep_the_old_location_working`),
  and `dse groups` prints which file it used. Suite, clippy and fmt green.
  The repository directory is still `dsh/`; renaming the folder is the user's call.
