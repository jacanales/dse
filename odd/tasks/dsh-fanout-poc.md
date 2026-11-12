# dsh — fan-out shell PoC

Rust TUI that fans one command out to a group of hosts, borrowing connection settings from the user's existing OpenSSH configuration.
PoC scope: runnable end-to-end (config, fan-out engine, live TUI), not polished.

## Specs

- **S1** — Terminal application, similar to dsh, to manage and send commands to several machines, machine groups or clusters at once. Quote: "Quiero tener una aplicación de terminal parecida a dsh para poder gestionar y enviar comandos a varias máquinas, grupos de máquinas o clusters de una sola vez."
- **S2** — An embedded sub-terminal (prompt inside the app): typing a command and pressing Enter runs it on every host of the selected group. Quote: "que me abra una subterminal, escribir "apt update", y ejecute ese comando en todas las máquinas del grupo de máquinas que haya elegido".
- **S3** — Live, pretty, modern and elegant progress per host while a command runs, showing failures and errors. Quote: "cuando lance un comando, quiero ver el progreso, de una forma bonita, moderna y elegante, si falla, y si hay error".
- **S4** — Rust, using its strengths (async fan-out, no shell glue). Quote: "Quiero que uses rust y la potencia de rust para hacer esto."
- **S5** — No connection settings inside the app: hosts are plain OpenSSH targets resolved from the user's SSH config (key auth already working). A group is just a list of hosts, e.g. `pve00.jacd.local, pve01.jacd.local, pve02.jacd.local`. Quotes: "en lugar de configurar el modo de conexión en la propia app, aprovecharemos la configuración de ssh", "para crear un grupo de máquinas simplemente tendremos que configurar: "pve00.jacd.local, pve01.jacd.local, pve02.jacd.local" y como ya tengo SSH configurado para conectar por clave a esas máquinas, podrá conectarse".
- **S6** — First deliverable is an initial PoC, explicitly a base to keep polishing into a fast, modern, easy tool. Quote: "Con esta información crea una PoC inicial y luego la iremos puliendo para tener una herramienta facil de utilizar, rápida y moderna."
- **S7** — Borrow feature ideas from pssh/pdsh (parallel execution, per-host output labeling, exit-code summary, non-interactive auth). Quote: "Algunos proyectos similares que hacen esto y de los que puedes coger ideas son: pssh, pdsh."

### PoC decisions (S5, S6)

- Connection layer = the system `ssh` client, so `~/.ssh/config`, keys, agent, `ProxyJump`, `KnownHosts` all apply with zero reimplementation. Effective settings are read back with `ssh -G <target>` for display only.
- Groups live in `~/.config/dsh/config.toml` (`[groups]`), one flat list per group; `@group` entries nest groups. No host credentials in that file.
- Binary name `dsh` (repo/project name). Installing it puts `dsh` ahead of the Debian `dsh` (`/opt/homebrew/bin/dsh`) in PATH; rename the `[[bin]]` if both must coexist.

## Tasks

- **T1** — *S5* — Config + target resolution + SSH resolution (`src/config.rs`, `src/ssh.rs`), with tests. Commit: `feat(config)`.
- **T2** — *S2, S4, S7* — Fan-out engine: concurrent bounded execution, per-host stdout/stderr/exit events, summary + exit code (`src/exec.rs`), tests against a stub ssh. Commit: `feat(exec)`.
- **T3** — *S3* — TUI: prompt line, per-host live status, progress gauge, detail view (`src/tui/`). Commit: `feat(tui)`.
- **T4** — *S1, S2, S6* — CLI wiring: `dsh` (TUI), `dsh run`, `dsh groups`, `dsh hosts` (`src/main.rs`, `src/cli.rs`). Commit: `feat(cli)`.
- **T5** — *S6* — README with config example and keybindings; `cargo fmt`, `cargo clippy -D warnings`, `cargo test`. Commit: `docs`.

## Log

- **L1** (user, verbatim):

```text
Quiero tener una aplicación de terminal parecida a dsh para poder gestionar y enviar comandos a varias máquinas, grupos de máquinas o clusters de una sola vez. Por ejemplo, que me abra una subterminal, escribir "apt update", y ejecute ese comando en todas las máquinas del grupo de máquinas que haya elegido. Además cuando lance un comando, quiero ver el progreso, de una forma bonita, moderna y elegante, si falla, y si hay error.

Quiero que uses rust y la potencia de rust para hacer esto.

Algunos proyectos similares que hacen esto y de los que puedes coger ideas son: pssh, pdsh.

Para conectar a las máquinas, en lugar de configurar el modo de conexión en la propia app, aprovecharemos la configuración de ssh. Es decir, para crear un grupo de máquinas simplemente tendremos que configurar: "pve00.jacd.local, pve01.jacd.local, pve02.jacd.local" y como ya tengo SSH configurado para conectar por clave a esas máquinas, podrá conectarse. Con esta información crea una PoC inicial y luego la iremos puliendo para tener una herramienta facil de utilizar, rápida y moderna.
```
