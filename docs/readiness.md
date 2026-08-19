# Readiness dependencies

A command can wait for one or more other commands before its pre-steps and main command run. Starting the dependent command recursively starts its dependencies first.

```toml
[[groups.commands]]
name = "dashboard"
run = "yarn start"

[[groups.commands.wait_for]]
command = "api"
kind = "keyword"
value = "ready to accept connections"
case_sensitive = false
timeout = 60
```

Blade supports three readiness kinds:

- `keyword` waits until a dependency output line contains `value`.
- `idle` waits until a running dependency has produced no command output for `seconds`.
- `delay` waits until a dependency has been running for `seconds`.

Each condition has its own `timeout`, which defaults to 60 seconds. Set `timeout = 0` to wait indefinitely. Conditions are checked in configuration order, and every condition must pass before the dependent command begins its pre-steps.

## Force-starting a waiting command

Select a command in the `waiting` state and press `f` to bypass all of its remaining readiness conditions. Blade immediately continues with that command's pre-steps and then its main command.

When a group or project row is selected, `f` force-starts every command in that selection that is currently waiting. Commands that are stopped, preparing, running, or stopping are left unchanged.

Force start is a one-time runtime action:

- It does not change the `.blade` configuration.
- It does not stop dependencies that Blade already started.
- It does not mark the readiness conditions as permanently satisfied.
- The conditions are checked normally the next time the command starts or restarts.
- It cannot start a stopped command; press `Enter` or `s` first, then use `f` if the command remains waiting.

Blade writes `force start requested; bypassing remaining readiness conditions` to the command's output so the override remains visible in its logs.

Use force start when a dependency is usable but its configured readiness signal is missing or incorrect. If it is needed regularly, update the readiness condition instead of relying on the override.

## Cross-project dependencies

Combined sessions can reference another project with `Project Alias::command`:

```toml
[[groups.commands.wait_for]]
command = "Backend::api"
kind = "keyword"
value = "ready"
timeout = 120
```

The same force-start behavior applies: only the waiting dependent bypasses its remaining conditions, while the cross-project dependency keeps running. See [Global project list](projects.md#cross-project-dependencies) for validation and launch requirements.
