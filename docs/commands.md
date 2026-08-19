# Managing commands from the shell

Blade can list, add, edit, and delete project commands without opening the project file manually. Each operation discovers the nearest project `.blade`; it can also use a global project list as its entry point. `blade command list` also displays nested [command actions](actions.md).

## Listing configuration

Display the discovered local project in a readable form:

```sh
blade command list
```

The output includes the project theme, log-buffer size, groups, commands, run commands, working directories, shell, pre-steps, readiness dependencies, shutdown timeout, autostart state, and logfile rotation. For example:

```
Project: My app
  File: /home/user/project/.blade
  Theme: nord
  Max log lines: 100000
  Groups: 2
    Project (2 commands):
      - api [autostart]
        Run: yarn dev:api
        Cwd: /home/user/project
        Shell: /bin/zsh
        Stop timeout: 10s
        Pre-steps:
          yarn install --frozen-lockfile
        Waits for:
          database: log keyword "ready to accept connections" (case-insensitive), timeout 60s
      - dashboard [autostart]
        Run: yarn dev
        Cwd: /home/user/project/frontend
        Shell: /bin/zsh
        Stop timeout: 10s
        Waits for:
          api: output idle for 2s, timeout 90s
    Frontend (1 commands):
      - worker [manual]
        Run: node worker.js
        Cwd: /home/user/project
        Shell: /bin/zsh
        Stop timeout: 10s
```

When pointed at a global list it also shows the global theme, described custom themes, registered aliases, and every referenced project:

```sh
blade --file ~/.blade command list
blade --file ~/.config/blade.conf command list
blade --file ~/.blade command list --project "ACME Development"
```

## Interactive usage

Run an operation without its required details to start a prompt-driven workflow:

```sh
blade command add
blade command edit
blade command delete
```

Add asks for the group, command name, run command, working directory, autostart setting, and zero or more pre-steps. Entering a group name that does not exist creates the group. Edit and delete display the available commands before asking which one to change.

## Scriptable usage

Provide the required add fields to avoid prompts:

```sh
blade command add api \
  --group Backend \
  --run "source venv/bin/activate && ./manage.py runserver" \
  --cwd burgundy \
  --pre "source venv/bin/activate && poetry install --no-root" \
  --pre "source venv/bin/activate && ./manage.py migrate" \
  --autostart
```

`--pre` may be repeated. When omitted in non-interactive mode, `cwd` defaults to `.` and both pre-steps and autostart default to disabled.

Edit only the fields supplied on the command line:

```sh
blade command edit api --run "./manage.py runserver 0.0.0.0:8000"
blade command edit api --rename web --autostart false
blade command edit api --pre "poetry install" --pre "./manage.py migrate"
blade command edit api --clear-pre
```

Renaming a command also updates unqualified `wait_for.command` references in the same project file. Cross-project references in other files cannot be rewritten automatically; update those references and run `blade validate` on the global project list.

Delete prompts for confirmation unless `--yes` is supplied:

```sh
blade command delete api
blade command delete api --yes
```

Blade refuses a deletion that would leave a local dependency pointing at a missing command.

## Managing commands inside the TUI

Press `M` to open **Manage configured commands and actions**. This screen edits the project files loaded in the current Blade session:

Commands are displayed hierarchically under their project and group, with actions nested beneath their parent command, matching the structure of the Commands pane.

| Key | Action |
| --- | --- |
| `c` | Add a command |
| `a` | Add an action under the selected command |
| `e`, `Enter` | Edit the selected command or action |
| `m` | Move the command or reassign the action to another parent |
| `K`, `J` | Reorder within the current group or parent command |
| `d` | Delete after confirmation |
| `Esc` | Close the manager or cancel the current form |

The command form supports the command name, run command, working directory, project, and group. The action form supports name, run command, an optional inherited-directory override, parent command, stopped-parent requirement, and restart policy. Editable rows are labeled `[type]`, long values wrap within the popup, and the selected text row shows a movable `█` cursor. `Tab` selects a field. In text fields, left/right moves by character, Home/End jumps to an edge, Backspace removes the preceding character, and Delete removes the following character. Choice fields use left/right; the stopped-parent field also accepts Space.

If Apply fails because required fields are empty or the proposed configuration is invalid, Blade shows the full error in a modal popup and leaves the form intact. Press `Enter` or `Esc` to return; for missing fields, the cursor moves to the first one.

Each operation is validated and written atomically as soon as `Enter` applies it. There is no later unsaved batch. Editing an active command changes the configuration used on its next start without replacing the process already running. Active commands must be stopped before deletion. Reordering is limited to the current group.

Moving between project files preserves the command's configuration, but Blade rejects a cross-project move when the command has readiness dependencies whose meaning could change. Remove or qualify those dependencies first. A move is also rejected when removing the command would leave a dependency invalid. If the moved command was the final command in its old group, Blade removes that empty group while keeping the project available as a management destination.

Delete opens a modal confirmation showing the selected command or action. Confirm with `y` or `Enter`, or cancel with `n` or `Esc`. Deleting the final command in a group removes the now-empty group while retaining the project as a management destination. Deleting a command also deletes its nested actions after explicitly showing their count in the confirmation.

For one-off commands that may later become configuration, see [Ephemeral commands](ephemeral.md).

## Targeting another project

`--file` is global and may appear before or after the subcommands:

```sh
blade --file /home/user/acme/.blade command edit api
blade command add dashboard --file /path/to/project/.blade --group Frontend --run "yarn start"
```

Global project lists (`~/.blade`, `~/.config/blade.config`, and `~/.config/blade.conf`) may also be passed directly. Select the referenced project by its global alias:

```sh
blade --file ~/.blade command edit api \
  --project "ACME Development" \
  --run "./manage.py runserver 0.0.0.0:8000"

blade --file ~/.config/blade.conf command add worker \
  --project "Backend" \
  --group Workers \
  --run "celery -A app worker"
```

When the global list contains one project, `--project` is optional. When it contains several, Blade requires the alias rather than guessing. `--project` without `--file` uses the active global list, so `blade command edit api --project Backend ...` works from any directory.

Add, edit, and delete mutate the selected project's referenced `.blade`; project registration, aliases, global themes, and custom-theme definitions remain in the global file. This keeps the global list strict while making it a convenient command-management entry point.

## Registering the current project

Register the discovered project in Blade's global picker:

```sh
blade command register
blade command register --name "ACME Development"
```

The alias defaults to the project's `name`. Blade updates the first existing global list according to normal precedence (`~/.blade`, then `~/.config/blade.config`, then `~/.config/blade.conf`), or creates `~/.blade` when none exists. Use `--registry` to choose the destination explicitly:

```sh
blade command register --registry ~/.config/blade.config
```

Registration is idempotent when the same path and alias already exist. Duplicate aliases and attempts to register one path under conflicting aliases are rejected. Blade validates every referenced project and the combined dependency graph before saving the global list.

Deregister the discovered project with confirmation, or use `--yes` for scripts:

```sh
blade command deregister
blade command deregister --yes
blade command deregister --registry ~/.config/blade.config
```

Blade locates the entry by its canonical project path, validates the remaining combined dependency graph, and refuses removal when another registered project still depends on it. If the removed project was the final entry, Blade removes the now-empty global list file.

You can also point `--file` at the global list itself. Blade displays the registered projects and interactively asks which one to remove before showing the normal confirmation:

```sh
blade --file ~/.blade command deregister
blade --file ~/.config/blade.conf command deregister
```

For scripts, select the alias explicitly and bypass confirmation:

```sh
blade --file ~/.blade command deregister \
  --project "ACME Development" \
  --yes
```

## Write safety

Blade preserves unrelated TOML, comments, themes, readiness blocks, and custom command settings. It writes the proposed change to a sibling temporary file, validates it, preserves the original file permissions, and atomically replaces the project file only after validation succeeds. A failed operation leaves the original file unchanged.

The shell editor currently manages `name`, `run`, `cwd`, `pre`, and `autostart`. Existing readiness, logging, rotation, and shutdown fields are preserved and can still be edited directly in TOML.
