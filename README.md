# Blade

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Blade ("the runner") is a terminal UI for starting and supervising all the commands that make up a project. Commands live in a `.blade` TOML file, are organized into groups, and run through your interactive login shell so shell environment variables, aliases, functions, and builtins remain available.

Blade is Linux/Unix-oriented and currently supports POSIX-compatible user shells such as zsh and bash.

## Features

- **Interactive TUI** -- start, stop, restart, and monitor commands in a terminal interface
- **Dependency management** -- commands wait for readiness conditions (keyword, idle, delay) before starting
- **Grouped commands** -- organize related commands into named groups
- **Shell integration** -- runs through your login shell, preserving environment, aliases, and functions
- **Theme support** -- 17 built-in themes with custom theme and color override support
- **Ephemeral commands** -- run one-off commands without editing config files
- **Nested actions** -- maintenance commands (migrations, installs) that inherit parent context
- **Global project list** -- launch multiple projects in a combined session
- **Systemd integration** -- manage system services with lifecycle-aware wrappers
- **Logging** -- file-based logging with rotation, in-memory ring buffers, and output dump

## Quick start

```sh
# Install (requires Rust 1.97+)
make build && sudo make install

# Create a project config
cd my-project
blade init

# Start the TUI
blade
```

## Install

Rust 1.97 or newer is required. From this repository:

```sh
make build
sudo make install
```

`make build` creates the optimized binary at `target/release/blade`. `make install` copies that existing binary to `/usr/local/bin/blade` and deliberately does not run Cargo under `sudo`. Override the destination with `PREFIX`, for example `make install PREFIX="$HOME/.local"`, or use `DESTDIR` when building a package.

For a statically linked Linux binary suitable for distribution, install the Rust musl target and use the static target:

```sh
rustup target add x86_64-unknown-linux-musl
make static
sudo make install-static
```

The binary is written to `target/x86_64-unknown-linux-musl/release/blade`. `make install-static` installs that binary, while the regular `make install` continues to install `target/release/blade`. Both install targets support `PREFIX` and `DESTDIR`. Cross-build another architecture by overriding `STATIC_TARGET` for both commands, for example `make static STATIC_TARGET=aarch64-unknown-linux-musl` followed by `sudo make install-static STATIC_TARGET=aarch64-unknown-linux-musl`; the corresponding Rust target and linker must be installed.

Alternatively, install directly into Cargo's per-user binary directory with `cargo install --path .`.

For development, `cargo run -- --help` works without installing the binary.

Blade uses `xclip` for clipboard actions and the coreutils `timeout` command to expire clipboard ownership after two minutes. Everything else is handled by the Blade binary and the configured user shell.

## Start a project

Run the interactive initializer in the project directory:

```sh
blade init
blade validate
blade
blade run                 # explicit subcommand (same as running blade with no subcommand)
blade --global            # skip the local .blade and open the global picker
blade --all               # start every command, overriding autostart settings
blade command list        # show the discovered configuration clearly
blade command add         # interactively append a command
```

Blade searches the current directory and its parents for `.blade`, so it can be launched from a nested source directory. Use `blade --file path/to/config` to select one explicitly, or `blade --global` to skip local discovery and open the global project picker. When no project file is found, Blade reads `~/.blade`, `~/.config/blade.config`, or `~/.config/blade.conf` and opens that picker automatically. The global list may also define the fallback theme, custom inline or file-referenced themes, and the theme used for an **All projects** session; see [Global project list](docs/projects.md) and [Themes](docs/themes.md#custom-global-themes). An existing project config is never overwritten by `blade init` without confirmation (or an explicit `--force`).

The `run` subcommand is the explicit form of the default action. It accepts the same startup options:

```sh
blade run                 # start the TUI with normal autostart behavior
blade run --global        # open the global project picker
blade run --all           # start every command regardless of autostart
blade run --no-autostart  # open without starting anything
```

Normally Blade starts only commands with `autostart = true`. Use `--all` (also available as `--start-all`) to start every command when the TUI opens, regardless of its configured `autostart` value. Use `--no-autostart` to open without starting anything.

## Configuration

`.blade` is strict TOML. Command names must be unique across the whole project because readiness dependencies refer to commands by name.

```toml
version = 1
name = "My app"
shell = "/bin/zsh"          # defaults to $SHELL
log_dir = ".blade-logs"     # optional: logs every command to <name>.log
log_rotate_bytes = 10485760  # optional: rotate each logfile at 10 MiB
log_rotate_keep = 5          # retain cmd.log.1 through cmd.log.5
max_log_lines = 100000       # bounded in-memory output per command
stop_timeout = 10            # default seconds between stop-signal escalations

[theme]
preset = "nord"              # see docs/themes.md for all presets
# accent = "#88c0d0"         # optional override applied after the preset

[[groups]]
name = "Project"

[[groups.commands]]
name = "api"
run = "yarn dev:api"
cwd = "."
shell_setup = ["source .venv/bin/activate"] # shared by run, pre-steps, and actions
pre = ["yarn install --frozen-lockfile"]
autostart = true
log_file = ".blade-logs/api.log" # optional; overrides log_dir for this command
log_rotate_bytes = 52428800       # optional per-command override
log_rotate_keep = 3               # optional per-command override
stop_timeout = 30                 # optional per-command override

[[groups.commands.wait_for]]
command = "database"
kind = "keyword"
value = "ready to accept connections"
case_sensitive = false
timeout = 60

[[groups.commands.actions]]
name = "Install dependencies"
run = "yarn install --frozen-lockfile"
requires_stopped = true
restart_after = "if-running"

[[groups]]
name = "Frontend"

[[groups.commands]]
name = "dashboard"
run = "yarn dev"
cwd = "frontend"

[[groups.commands.wait_for]]
command = "api"
kind = "idle"
seconds = 2
timeout = 90
```

The complete example at [examples/full.blade](examples/full.blade) demonstrates groups, command-specific logs, shared shell setup, nested actions, pre-steps, autostart, and all readiness modes.

Projects launched together from the global project list can use qualified dependencies such as `command = "Backend::api"`. See [Global project list](docs/projects.md#cross-project-dependencies) for composition and validation rules.

The optional `[theme]` table selects a built-in palette and can override individual colors using names, `#RRGGBB`, or `ansi:N`. See [Themes](docs/themes.md) for all fields and formats.

System services need a lifecycle wrapper rather than a bare `systemctl start`, because `systemctl` returns as soon as the unit starts. See [Running systemd services](docs/systemd.md) for a wrapper that follows the journal, detects a unit entering the failed state, and stops only services that Blade started.

When `log_rotate_bytes` is set, Blade checks the logfile before every append. A write that would cross the limit rotates `cmd.log` to `cmd.log.1`, shifts older backups upward, and removes backups beyond `log_rotate_keep` (default `5`). Rotation settings are inherited by every command and may be overridden per command. Rotation only applies when `log_dir` or `log_file` enables file logging; the in-memory `max_log_lines` ring remains independent.

### Shell behavior and pre-steps

Blade invokes the configured shell as an interactive login shell (`-ilc`) attached to its own pseudo-terminal. The PTY makes terminal-aware programs emit line-buffered output immediately instead of holding output in a pipe buffer. Every command's `shell_setup`, `pre`, and `run` entries execute sequentially in one shell session, so setup may export a variable or activate a virtual environment. `shell_setup` is also inherited by nested actions; command-specific `pre` steps are not. A failed setup or pre-step prevents `run` from executing.

The shell process starts a new Unix process group. Signals are sent to that entire group, including grandchildren, rather than only to the shell wrapper.

### Readiness dependencies

Starting a command recursively starts every command it waits for. All its readiness conditions must pass before its pre-steps begin:

- `keyword`: wait until a dependency output line contains `value`. Matching can be case-sensitive or insensitive.
- `idle`: wait until a running dependency has produced no command output for `seconds`.
- `delay`: wait until a dependency has been running for `seconds`.

`timeout` defaults to 60 seconds per condition. Set it to `0` to wait indefinitely. Blade reports missing dependency names and dependency cycles as validation errors rather than entering a deadlock at runtime.

Idle and delay are useful fallbacks, but a stable readiness keyword is the strongest signal when the underlying service provides one.

Press `f` while a command is waiting to bypass all of its remaining readiness conditions and begin its pre-steps immediately. On a selected group or project, `f` applies to every command currently waiting. This does not stop dependencies that Blade has already started. See [Readiness dependencies](docs/readiness.md#force-starting-a-waiting-command) for the full behavior and caveats.

## TUI keys

| Key | Action |
| --- | --- |
| `Tab` | Switch focus between commands and output |
| `→`, `←` | Focus panes while wrapping is on; scroll horizontally while it is off (`←` returns to commands from the left edge) |
| `↑`/`↓`, `j`/`k` | Select a group, command, or output line |
| `Ctrl-P` | Open Quick jump and fuzzy-find a command or nested action to focus |
| `:` | Run a one-off command in the selected project context and add it to the Ephemeral group |
| `p`, `D` | Persist the selected ephemeral command to a project, or remove it once stopped |
| `M` | Manage configured commands and nested actions |
| `Enter`, `s` | Start the selected command or every command in the selected group |
| `f` | Force-start selected commands that are waiting for dependencies |
| `x` | Gracefully stop the selected command/group; repeat to send terminate, then kill |
| `r` | Gracefully restart the selected command/group; repeat while stopping to force it |
| `PgUp`/`PgDn`, `g`/`G` | Page output or jump to first/last line |
| `w` | Toggle output wrapping (on by default) |
| `h`/`l` | Scroll horizontally while wrapping is off |
| `/` | Start a case-sensitive, literal substring search |
| `\` | Start a case-insensitive regex search |
| `n`/`N` | Select the next/previous search match |
| `t` | Toggle timestamps (enabled by default) |
| `T` | Preview themes and save the selected preset or custom-theme file reference |
| `y`, `Y` | Copy the selected line or all output with `xclip` (expires after two minutes) |
| `c` | Clear the selected command's in-memory output pane without truncating its logfile |
| `d` | Dump all output to a new file (existing files are not overwritten) |
| `q`, `Ctrl-C` | Gracefully stop everything and quit; repeat to force quit |
| `?` | Show the in-app key reference |

The layout changes automatically: wide terminals use side-by-side panes, narrow terminals stack commands above output, and terminal resize events are handled immediately. Scrollable Commands, Output, and management views show a themed vertical track with direction arrows and a position thumb whenever content exceeds the viewport.

Nested actions and ephemeral commands use the same process supervision, output pane, search, restart, stop escalation, and shutdown cleanup as configured commands. Actions retain independent logs and are not included in autostart or `--all`; see [Command actions](docs/actions.md).

## Validation

```sh
blade validate
blade validate path/to/.blade
```

Validation catches malformed TOML, wrong field types, unsupported versions, duplicate groups or commands, missing run commands, invalid readiness settings, missing dependency commands, self-dependencies, and dependency cycles. Unknown keys, projects without groups, empty groups, and missing working directories are warnings.

## Process lifecycle

The first stop sends `SIGINT` to the command's process group. Repeating stop sends `SIGTERM`, then `SIGKILL`. If no repeated key is pressed, Blade automatically escalates after `stop_timeout` so a stuck process cannot keep shutdown open forever. The project-level value defaults to 10 seconds and each command may override it. The timeout applies to each escalation stage, and the output title shows a countdown to the next signal.

Quit applies the same graceful stop to every active command. Pressing quit again immediately force-kills all remaining process groups. Blade restores the terminal before returning control to the shell.

## Documentation

- [Managing commands from the shell](docs/commands.md) -- interactive and scriptable add, edit, and delete operations.
- [Ephemeral commands and in-app management](docs/ephemeral.md) -- one-off commands and editing project commands without leaving the TUI.
- [Readiness dependencies](docs/readiness.md) -- dependency conditions, timeouts, and force-starting a waiting command.
- [Global project list](docs/projects.md) -- launching registered projects when the current directory has no `.blade`.
- [Themes](docs/themes.md) -- built-in palettes, described custom themes, file references, and project color overrides.
- [Running systemd services](docs/systemd.md) -- service ownership, journal output, Polkit authentication, shutdown, and dependencies.
- [Command actions](docs/actions.md) -- nested maintenance commands, inherited execution context, independent logs, and parent restart policies.

### Examples

- [examples/full.blade](examples/full.blade) -- complete application example.
- [examples/systemd.blade](examples/systemd.blade) -- copyable system-service example.

## Contributing

Contributions are welcome! Please open an issue or submit a pull request at [github.com/anomalyco/blade](https://github.com/anomalyco/blade).

## License

The MIT License (MIT)

Copyright (c) 2026 Adnan Hajdarevic
