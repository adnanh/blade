# Ephemeral commands and in-app management

Ephemeral commands are one-off commands created while Blade is running. They participate in the normal runner lifecycle but do not change `.blade` unless you explicitly persist them.

## Run a one-off command

Press `:`, type a shell command, and press `Enter`. Blade creates an **Ephemeral** group at the top of the Commands pane, starts the new command immediately, and focuses it.

The command uses the context of the currently selected command or group: its project, interactive login shell, working directory, log-buffer limit, and graceful-stop timeout. It deliberately does not copy pre-steps, readiness dependencies, autostart, or logfile settings. This keeps a one-off command independent and prevents it from unexpectedly writing a persistent log.

After creation, all normal controls apply:

- `Enter`/`s`, `x`, and `r` start, stop, and restart it.
- Output wrapping, paging, searching, copying, clearing, and dumping work normally.
- `Ctrl-P` includes ephemeral commands in Quick jump.
- Quitting Blade gracefully stops ephemeral processes along with configured processes; repeating quit force-terminates any remaining process groups.

Select a stopped ephemeral command and press `D` to remove it from the session. Blade refuses to remove an active command so its process cannot be orphaned.

## Persist an ephemeral command

Select it and press `p`. The dialog lets you choose:

- the permanent command name;
- the target project when several projects are open;
- an existing or new group.

Use `Tab` to select a field. Left/right moves the cursor within Name, left/right cycles Project, and `Alt-←/→` cycles existing groups while Group remains editable.

Press `Enter` to validate and atomically update the target `.blade`. The command moves out of **Ephemeral** into the configured group without losing its current output or runtime state. Its resolved shell and working directory are reloaded from the target project configuration.

The persisted command defaults to `autostart = false`. Use `M`, `blade command edit`, or direct TOML editing for options not present in the persistence dialog, such as pre-steps, readiness dependencies, logfile rotation, and per-command shutdown timeouts.

## Manage configured commands

Press `M` to add, edit, move, reorder, or delete configured commands without leaving the TUI. The full control list and write-safety behavior are documented in [Managing commands from the shell](commands.md#managing-commands-inside-the-tui).
