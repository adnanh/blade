# Running systemd services with Blade

Blade can manage a systemd service by treating its lifecycle as a command. The command starts the unit in a pre-step, follows its journal for output, and stops the unit when Blade stops the command.

Blade does not currently have a native `systemd_unit` configuration field. Do not use only `run = "systemctl start example.service"`: `systemctl start` returns after startup, so Blade would mark the command completed and could not supervise or stop the service.

## Complete system-service command

This example starts RabbitMQ while preserving a service that was already running before Blade opened:

```toml
[[groups.commands]]
name = "RabbitMQ"
pre = ['''
if systemctl is-active --quiet rabbitmq.service; then
  export BLADE_RABBITMQ_OWNED=0
else
  systemctl start rabbitmq.service
  export BLADE_RABBITMQ_OWNED=1
fi
''']
run = '''
blade_rabbitmq_cleanup() {
  trap - INT TERM EXIT
  if [ "$BLADE_RABBITMQ_OWNED" = "1" ]; then
    systemctl stop rabbitmq.service
  fi
}
trap blade_rabbitmq_cleanup INT TERM EXIT

blade_watch_systemd_unit() {
  blade_unit="$1"
  journalctl --follow --unit "$blade_unit" --lines=0 --output=cat &
  blade_journal_pid=$!

  while kill -0 "$blade_journal_pid" 2>/dev/null; do
    blade_unit_state=$(systemctl show "$blade_unit" --property=ActiveState --value 2>/dev/null) || blade_unit_state=unknown
    case "$blade_unit_state" in
      active|activating|reloading|deactivating) sleep 0.5 ;;
      inactive) blade_unit_status=0; break ;;
      failed)
        echo "$blade_unit entered the failed state"
        blade_unit_status=1
        break
        ;;
      *)
        echo "could not determine the state of $blade_unit"
        blade_unit_status=1
        break
        ;;
    esac
  done

  if ! kill -0 "$blade_journal_pid" 2>/dev/null; then
    wait "$blade_journal_pid"
    blade_journal_status=$?
    echo "journal follower for $blade_unit exited unexpectedly (status $blade_journal_status)"
    blade_unit_status=1
  else
    kill "$blade_journal_pid" 2>/dev/null || true
    wait "$blade_journal_pid" 2>/dev/null || true
  fi
  return "${blade_unit_status:-1}"
}

blade_watch_systemd_unit rabbitmq.service
'''
stop_timeout = 30
```

The ownership variable matters:

- If Blade starts RabbitMQ, stopping the command or quitting Blade stops RabbitMQ.
- If RabbitMQ was already active, Blade only follows its logs and leaves it active on exit.
- If RabbitMQ enters systemd's `failed` state, the wrapper exits nonzero and Blade marks the command as failed.
- The pre-step and run command share one shell, so the exported ownership variable remains available to the cleanup function.

`journalctl --follow` stays alive when a unit fails, so using it alone makes the command appear to keep running. The watcher polls the unit's `ActiveState` independently and uses the journal process only for output.

The longer `stop_timeout` gives `systemctl stop` time to complete before Blade escalates its process-group signals. It does not change systemd's own unit stop timeout.

## Authentication

Run `systemctl` directly and let Polkit authorize it. Do not put passwords in `.blade`.

In a graphical session, a Polkit authentication agent displays the authorization dialog outside the TUI. For i3, start an agent from the i3 configuration, for example:

```text
exec --no-startup-id /usr/lib/polkit-gnome/polkit-gnome-authentication-agent-1
```

Blade does not currently forward keyboard input to command PTYs. An inline `sudo` password prompt therefore cannot be answered and will appear to hang. Pre-authorizing with `sudo -v` is also unreliable for a long session because the credential may expire before shutdown.

For unattended use, prefer a narrowly scoped Polkit rule that permits only the required units and operations. Put custom policy in a new file under `/etc/polkit-1/rules.d/`; check that the chosen path does not already exist before creating it. Never grant unrestricted passwordless access to `systemctl`.

An example rule for one local user and one unit is:

```javascript
polkit.addRule(function (action, subject) {
  var verb = action.lookup("verb");
  var allowedVerb = verb === "start" || verb === "stop" || verb === "restart";

  if (
    action.id === "org.freedesktop.systemd1.manage-units" &&
    action.lookup("unit") === "rabbitmq.service" &&
    allowedVerb &&
    subject.user === "YOUR_USER" &&
    subject.local &&
    subject.active
  ) {
    return polkit.Result.YES;
  }
});
```

Replace `YOUR_USER` before installing the rule. System policy changes should be reviewed carefully because they authorize privileged service operations.

## User services

User units need no administrator authorization. Use `--user` consistently:

```toml
[[groups.commands]]
name = "Background worker"
pre = ["systemctl --user start example-worker.service"]
run = '''
trap 'systemctl --user stop example-worker.service; exit' INT TERM
# Use the same blade_watch_systemd_unit function as above, adding --user to
# both journalctl and systemctl inside it.
blade_watch_systemd_unit example-worker.service
'''
```

Use the ownership-aware pattern from the system-service example if Blade must preserve a user service that was already active.

## Dependencies and readiness

Starting a dependent command makes Blade start the service command automatically. `systemctl start` waits until a `Type=notify` unit reports that it is active, so a short delay is sufficient for RabbitMQ:

```toml
[[groups.commands.wait_for]]
command = "RabbitMQ"
kind = "delay"
seconds = 0.1
timeout = 60
```

For units whose systemd active state does not mean application readiness, follow the journal and use a stable keyword instead:

```toml
[[groups.commands.wait_for]]
command = "My service"
kind = "keyword"
value = "Ready to accept connections"
case_sensitive = false
timeout = 60
```

Be careful with journal history. `--lines=0` considers only new output, which prevents an old readiness line from satisfying a new start but requires the service to emit the keyword after journal following begins.

`--output=cat` emits only the service message, omitting journal timestamps and metadata because Blade already timestamps every captured line. Remove that option when the journal's hostname, unit name, or other metadata is useful.

## Operational notes

- Test `systemctl start`, `systemctl stop`, and `journalctl --follow` from the same graphical session before adding them to Blade.
- Ensure the user can read the unit's journal. Otherwise `journalctl` exits and the cleanup trap stops a service owned by Blade.
- A failed or cancelled `systemctl start` makes the Blade pre-step fail, so dependents do not start.
- Pressing restart stops an owned service through the cleanup trap and then starts a fresh command shell.
- Validate the finished project with `blade validate`.
