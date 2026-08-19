# Command actions

Actions are named commands nested under a configured command. They are useful for maintenance operations that share the command's project context, such as pulling changes, installing dependencies, running migrations, or clearing caches.

```toml
[[groups.commands]]
name = "Dashboard"
cwd = "./dashboard"
shell_setup = ["source .venv/bin/activate"]
run = "yarn start"

[[groups.commands.actions]]
name = "Pull changes"
run = "git pull --ff-only"

[[groups.commands.actions]]
name = "Install dependencies"
run = "yarn install"
requires_stopped = true
restart_after = "if-running"
```

The Commands pane displays actions beneath their parent. Select an action and use the normal start, stop, restart, output search, copy, clear, and dump controls. Each action has its own state and log buffer, so its output does not mix with the parent's output. Actions are also available through Quick jump as `Command › Action`.

Actions are manual by design: project autostart and `--all` start parent commands but do not run their actions.

## Inheritance and overrides

An action inherits its parent's shell, working directory, `shell_setup`, log directory and rotation defaults, maximum log lines, and stop timeout. `shell_setup` runs in the action's shell before its own pre-steps and run command. The parent's `pre` steps are not inherited.

The following action fields can override inherited behavior:

```toml
[[groups.commands.actions]]
name = "Build assets"
run = "yarn build"
cwd = "./assets"
pre = ["yarn install"]
stop_timeout = 30
log_file = ".blade-logs/build-assets.log"
log_rotate_bytes = 10485760
log_rotate_keep = 3
```

Relative action paths are resolved from the project file's directory, just like command paths.

## Parent lifecycle policies

Set `requires_stopped = true` when an action must not overlap its parent process. If the parent is running, Blade gracefully stops it, shows the action as waiting, and starts the action after shutdown completes. The normal repeated-stop and automatic escalation behavior remains available.

`restart_after` controls what happens after the action succeeds:

| Value | Behavior |
| --- | --- |
| `"never"` | Leave the parent stopped or unchanged. This is the default. |
| `"if-running"` | Restart the parent only when it was running before the action started. |
| `"always"` | Start or restart the parent after success. |

Blade does not restart the parent after a failed or manually stopped action, allowing the failure to be inspected safely.

## Managing actions in the TUI

Press `M` to open command management. Commands and actions are displayed in the same project/group/command hierarchy.

| Key | Action |
| --- | --- |
| `c` | Add a command |
| `a` | Add an action under the selected command |
| `e`, `Enter` | Edit the selected command or action |
| `m` | Move a command, or reassign an action to another command |
| `K`, `J` | Reorder within the current group or parent command |
| `d` | Delete after confirmation |

The action form edits its name, run command, optional working-directory override, parent command, stopped-parent requirement, and restart policy. An empty Cwd field means inherit the parent's directory. Moving a command carries all of its actions; deleting a command confirms that its actions will also be deleted.

Run `blade validate` after manual edits. Validation rejects missing action names or run commands, duplicate action names under one parent, invalid restart policies, and invalid timeout or rotation values.
