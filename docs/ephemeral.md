# Ephemeral commands and in-app management

Ephemeral commands are one-off commands created while Blade is running. They participate in the normal runner lifecycle but do not change `.blade` unless you explicitly persist them.

## Run a one-off command

Press `:`, type a shell command, and press `Enter`. Blade creates an **Ephemeral** group at the top of the Commands pane, starts the new command immediately, and focuses it.

The command uses the context of the currently selected command or group: its project, interactive login shell, working directory, log-buffer limit, and graceful-stop timeout. It deliberately does not copy pre-steps, readiness dependencies, autostart, or logfile settings. This keeps a one-off command independent and prevents it from unexpectedly writing a persistent log.

After creation, all normal controls apply:

- `Enter`/`s`, `x`, and `r` open confirmations to start, stop, and restart it.
- Output wrapping, paging, searching, copying, clearing, and dumping work normally.
- `Ctrl-P` includes ephemeral commands in Quick jump.
- Confirming quit with `q` gracefully stops ephemeral processes along with configured processes; repeating quit and confirming force-terminates any remaining process groups. Confirming detach with `Q` instead preserves ephemeral commands, their processes, and their logs in the supervisor session.

Select a stopped ephemeral command and press `D` to remove it from the session. Blade refuses to remove an active command so its process cannot be orphaned.

## Fork into a scratchpad

Select a configured command, nested action, or existing ephemeral command and press `;`. Blade opens an in-memory fork prefilled with the source command's name, run command, working directory, and pre-steps. Edit a field directly and use `Tab` or the up/down arrows to move between fields. The name controls the label shown in the **Ephemeral** group; the persistence dialog still lets you choose a permanent configured name later.

The pre-step field edits one step at a time. Use `Alt-←/→` to cycle through steps, `Ctrl-N` to add one, and `Ctrl-D` to remove the selected step. Relative working directories are resolved from the source command's project root; absolute paths and `~/` paths are also accepted. Blade validates that the directory exists before starting the fork.

Press `Enter` to run the fork as a new command in the **Ephemeral** group, or `Esc` to cancel. The fork keeps the source shell, shell setup, log-buffer limit, stop timeout, and nested actions. It deliberately clears readiness dependencies, autostart, and file logging so experimentation cannot start dependency trees or write configured logfiles unexpectedly. The source command and `.blade` file remain unchanged.

Cloned actions appear beneath the scratchpad command. Select one and press `;` to edit its name, run command, working directory, or pre-steps in place. Its stopped-parent and restart-after behavior is retained from the source action. Running, stopping, restarting, searching, and viewing output work the same way as for configured actions.

Scratchpads use the normal lifecycle, output, removal, and persistence controls. Press `p` if an experiment should become a configured command; its cloned actions are persisted with it.

To revise a scratchpad or other ephemeral command, stop it, select it, and press `;` again. The same form now edits that command in place instead of creating another fork. Pressing `Enter` updates and starts it with the same runtime identity, so its existing output history remains in the pane. Active ephemeral commands must be stopped before an edit can be applied.

## Persist an ephemeral command

Select it and press `p`. The dialog starts with the ephemeral command's current name and lets you choose:

- the permanent command name;
- the target project when several projects are open;
- an existing or new group;
- whether the persisted command should autostart.

Use `Tab` to select a field. Left/right moves the cursor within Name, cycles Project, or toggles Autostart. `Alt-←/→` cycles existing groups while Group remains editable; Space also toggles Autostart.

Press `Enter` to validate and atomically update the target `.blade`. The command moves out of **Ephemeral** into the configured group without losing its current output or runtime state. Its resolved shell and working directory are reloaded from the target project configuration.

Choose **Autostart** in the persistence dialog with the left/right arrows or Space. It defaults to **No**, so saving an experiment does not make it run on Blade's next launch unless requested. Use `M`, `blade command edit`, or direct TOML editing for options not present in the persistence dialog, such as readiness dependencies, logfile rotation, and per-command shutdown timeouts.

## Manage configured commands

Press `M` to add, edit, move, reorder, or delete configured commands without leaving the TUI. The full control list and write-safety behavior are documented in [Managing commands from the shell](commands.md#managing-commands-inside-the-tui).
