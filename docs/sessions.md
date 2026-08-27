# Sessions, detaching, and live reload

Blade runs command processes in a persistent per-project supervisor. The terminal UI connects to that supervisor over a user-only Unix socket, so the UI can disappear without closing command PTYs or interrupting output collection.

## Detach and attach

Press `Q`, then confirm with `y` or `Enter`, to detach. The terminal is restored immediately while configured commands, ephemeral commands, scratchpads, nested actions, runtime state, and in-memory logs remain in the supervisor.

Reconnect in either of these ways:

```sh
blade                         # automatically reconnects when opening the same project
blade attach                  # works when exactly one detached session exists
blade attach <session-id>     # selects a session when several exist
blade attach "Project name"  # a unique project name also works
```

If several sessions match, Blade prints their names and ids. Session descriptors and sockets live under `$XDG_RUNTIME_DIR/blade`; when that variable is unavailable Blade uses a user-specific directory below the system temporary directory. The directory is mode `0700`.

Press `q` or `Ctrl-C` and confirm when the session should end. Blade gracefully stops its process groups, closes the supervisor, and removes the session socket and descriptor. Repeating quit and confirming performs the existing terminate/kill escalation.

## Reload the TUI

Press `R` and confirm to restore the terminal and replace the current UI process with the Blade binary currently present on disk. The replacement client attaches to the existing supervisor instead of starting commands again.

If the replacement binary cannot be executed, the current binary opens the UI again and the supervisor remains untouched. If the replacement UI crashes after starting, command processes still belong to the separate supervisor and can be recovered with `blade attach` or the previous Blade binary, provided it speaks the same session protocol.

The supervisor intentionally keeps running its original binary version until the session ends. This makes UI reload and failure recovery transactional without transferring live PTYs between process owners. Changes to the process-supervision implementation take effect in newly created sessions.

## Failure boundary

The supervisor is the durable owner of child processes and PTY masters. A TUI crash is recoverable; a supervisor crash is not currently recoverable and may cause terminal-oriented child processes to receive a hangup. Blade isolates the supervisor from the launching terminal session so ordinary detach, shell exit, and TUI failure do not trigger that condition.
