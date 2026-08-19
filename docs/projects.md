# Global project list

When `blade` is started without `--file`, it first searches the current directory and its parents for a project `.blade`. If none is found, Blade opens a project picker using the first global project list that exists:

1. `~/.blade`
2. `~/.config/blade.config`
3. `~/.config/blade.conf`

Run `blade --global` to skip the current directory and parent search and open this picker directly. The flag works with startup overrides such as `blade --global --all` and `blade --global --no-autostart`, and conflicts with `--file` because both explicitly choose the launch source.

The global file is strict TOML and contains named paths to project configuration files:

```toml
version = 1

[theme]
preset = "matrix-alt"

[[projects]]
name = "ACME Development"
path = "/home/user/acme/.blade"

[[projects]]
name = "Another project"
path = "~/projects/another/.blade"
```

Paths may point directly to a project file or to a directory containing `.blade`. Relative paths are resolved from the directory containing the global project list, and paths beginning with `~/` are resolved from the user's home directory.

The optional global `[theme]` table accepts the same presets and color overrides as a project theme. For a single-project session, a project with no `[theme]` inherits the global theme. Project color fields layer over the global theme when the project does not select its own preset; an explicit project preset takes precedence over the global theme. A project may also select a custom theme with `[theme] file = "path/to/theme.toml"`. The global file can define named inline or file-referenced custom themes under `[themes]`, including picker descriptions; see [Custom global themes](themes.md#custom-global-themes).

The picker includes an **All projects** entry above the individual projects. Selecting it opens every referenced project in one Blade session. The Commands pane keeps a three-level hierarchy—project, group, command—and selecting a project row allows start, stop, or restart to be applied to all of that project's commands. Runtime command identities and readiness dependencies are isolated per project, so projects may reuse the same command names safely.

Use `↑`/`↓` or `j`/`k` to select an entry, `Enter` to launch it, and `q` or `Esc` to cancel. Normal autostart settings are retained across a combined session. Run `blade --all` and select **All projects** to start every command in every project, or use `--no-autostart` to open the combined session without starting commands.

Run `blade validate` from a directory without a local project to validate the global list, every referenced project, and the fully resolved combined dependency graph. `blade validate ~/.blade` validates the global list explicitly.

Use `blade --file ~/.blade command list` (or point `--file` at `~/.config/blade.config`/`blade.conf`) for a readable view of the global settings and referenced project commands. Command mutations can target a registered alias directly, for example `blade --file ~/.blade command edit api --project "ACME Development" --run "new command"`.

Combined sessions preserve each referenced project's shell, working directories, logging, ring-buffer size, and command shutdown settings. An **All projects** session always uses the global theme and ignores individual project themes, providing one consistent palette. Pressing `T` in that session previews themes and saves the selected preset to the global project-list file.

## Cross-project dependencies

A command may wait for a command in another referenced project by qualifying the command name with the project alias from the global list:

```toml
# In the project registered as "Frontend"
[[groups.commands.wait_for]]
command = "Backend::api"
kind = "keyword"
value = "Starting development server at"
timeout = 180
```

Here, `Backend` must exactly match a `[[projects]].name` value in the global project list, and `api` must match a command name in that project. All existing readiness kinds—`keyword`, `idle`, and `delay`—work across projects. Starting the dependent command automatically starts the referenced command first.

Force start also works across projects. Select a waiting dependent command and press `f` to bypass its remaining readiness checks without stopping the dependency that Blade already started. See [Readiness dependencies](readiness.md#force-starting-a-waiting-command).

Qualified dependencies require the **All projects** session because an individual project does not have the other project's runtime. Blade reports a validation error if such a project is launched alone. Missing targets, self-dependencies, and dependency cycles spanning multiple projects are rejected while the combined session is being built. On exit, Blade uses dependency order to stop dependents before their cross-project dependencies.

The exact `~/.blade` path is reserved for this global list and is not treated as a project discovered from a child directory. Explicit `blade --file ~/.blade` still treats it as a project file, so pass project `.blade` paths rather than the global list to `--file`.
