# Sessions, detaching, and live reload

Blade runs command processes in a persistent per-session supervisor. A normal project invocation has one session and supervisor for that project. An **All projects** invocation has one combined session and supervisor for the selected project set. Multiple TUI and CLI processes targeting that session are clients of the same supervisor; there is no single machine-wide Blade supervisor. The terminal UI connects over a user-only Unix socket, so the UI can disappear without closing command PTYs or interrupting output collection.

The supervisor is also the owner of complete in-memory log rings. Attached TUIs receive a bounded live tail and request 256-line pages while navigating history. Search executes in the supervisor and returns a page around the match; copy-all and dump stream a snapshot over dedicated connections so bulk output cannot stall normal status refreshes.

## Detach and attach

Press `Q`, then confirm with `y` or `Enter`, to detach. The terminal is restored immediately while configured commands, ephemeral commands, scratchpads, nested actions, runtime state, and in-memory logs remain in the supervisor.

Reconnect in either of these ways:

```sh
blade sessions                 # list running sessions and their ids
blade sessions --json          # structured output for scripts
blade                         # automatically reconnects when opening the same project
blade attach                  # works when exactly one detached session exists
blade attach <session-id>     # selects a session when several exist
blade attach "Project name"  # a unique project name also works
```

If several sessions match, Blade prints their names and ids. Session descriptors and sockets live under `$XDG_RUNTIME_DIR/blade`; when that variable is unavailable Blade uses a user-specific directory below the system temporary directory. The directory is mode `0700`.

Scripts and other applications can inspect and control a supervisor through `blade command list --session [SESSION]` and the `start`, `stop`, `restart`, `terminate`, and `kill` command operations. Add `--json` to the live list for structured state. These requests update the supervisor directly, so all attached TUIs display the change on their next refresh. See [Controlling a running session](commands.md#controlling-a-running-session).

Press `q` or `Ctrl-C` and confirm when the session should end. Blade gracefully stops its process groups, closes the supervisor, and removes the session socket and descriptor. Repeating quit and confirming performs the existing terminate/kill escalation.

The same operation is available without opening the TUI:

```sh
blade shutdown                    # the only running session
blade shutdown <session-id>       # choose by id or unique project name
blade shutdown <session-id> --force
```

Normal shutdown begins each command's `SIGINT` and configured timeout escalation, then exits the supervisor when every process has stopped. Attached TUIs display the stopping states and close when the supervisor exits. `--force` sends `SIGKILL` to active process groups and exits immediately.

## Reload the TUI

Press `R` and confirm to restore the terminal and replace the current UI process with the Blade binary currently present on disk. The replacement client attaches to the existing supervisor instead of starting commands again.

If the replacement binary cannot be executed, the current binary opens the UI again and the supervisor remains untouched. If the replacement UI crashes after starting, command processes still belong to the separate supervisor and can be recovered with `blade attach` or the previous Blade binary, provided it speaks the same session protocol.

The supervisor intentionally keeps running its original binary version until the session ends. This makes UI reload and failure recovery transactional without transferring live PTYs between process owners. Changes to the process-supervision implementation take effect in newly created sessions.

## Protocol compatibility

The feature-complete supervisor wire format shipped by Blade 1.0 is protocol 1. The initial handshake advertises both the protocol version and explicit capabilities such as paged logs, server-side search, live command status, and direct lifecycle signals.

Future protocol changes should remain additive. A newer frontend keeps a minimum supported protocol and connects to older supervisors within that range, using the advertised capability set to disable only unavailable features or select a fallback. A protocol version should be raised only when the wire format changes; ordinary Blade releases that keep the same request and response shapes continue using the existing version.

## Failure boundary

The supervisor is the durable owner of child processes and PTY masters. A TUI crash is recoverable; a supervisor crash is not currently recoverable and may cause terminal-oriented child processes to receive a hangup. Blade isolates the supervisor from the launching terminal session so ordinary detach, shell exit, and TUI failure do not trigger that condition.
