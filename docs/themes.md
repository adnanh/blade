# Themes

Blade can select a color palette globally or per project with an optional top-level `[theme]` table. In a project file, put it before the first `[[groups]]` table:

```toml
[theme]
preset = "nord"
accent = "#8fbcbb"
muted = "ansi:8"

[[groups]]
name = "Backend"
```

The same table may be added to `~/.blade`, `~/.config/blade.config`, or `~/.config/blade.conf` before the first `[[projects]]` entry. A single project inherits that global theme unless it configures its own preset. Project color fields without a preset layer over the global palette. When **All projects** is selected, Blade always uses the global theme and ignores project themes.

The available presets are:

- `default` (also accepted as `blade`) preserves Blade's original terminal colors.
- `red` uses bold crimson accent colors.
- `yellow` uses bright golden accent colors.
- `orange` uses warm amber accent colors.
- `matrix` uses phosphor green terminal colors, including green command output.
- `matrix-alt` keeps the Matrix controls and state colors but renders command output in white.
- `purple` uses rich violet accent colors.
- `blue` uses clear ocean blue accent colors.
- `gray` uses cool neutral gray controls with distinct command-state colors.
- `sand` uses soft warm desert colors.
- `nord` uses the Nord palette.
- `gruvbox` uses the Gruvbox palette.
- `dracula` uses vivid purple and neon colors.
- `catppuccin` uses the Catppuccin Mocha palette.
- `tokyo-night` uses deep blue nighttime colors.
- `solarized-dark` uses the Solarized dark palette.
- `monochrome` avoids hue-dependent state colors.

## Custom global themes

Named custom themes can be defined inline in the global configuration. Their `preset` must name a built-in theme, and any supplied colors override that base:

```toml
[themes.terminal-green]
preset = "matrix"
text = "white"
muted = "#237a3b"
description = "Green controls with crisp white output"

[theme]
preset = "terminal-green"
```

They can also reference separate files:

```toml
[themes]
ocean = "themes/ocean.toml"
company = "~/.config/blade/themes/company.toml"
```

Relative paths are resolved from the global configuration's directory. A referenced file may contain either flat theme fields:

```toml
preset = "blue"
accent = "#38bdf8"
text = "white"
description = "Bright cyan over deep ocean blues"
```

or the same fields inside a `[theme]` table. `description` is optional and is shown beside the theme name in the picker. Inline and referenced definitions can be mixed. Theme names use letters, numbers, `-`, or `_`, are case-insensitive, and cannot replace a built-in name.

Custom themes appear after a **Custom themes** divider in the `T` picker. In an **All projects** session, selecting one saves its name to the global `[theme]`. In a single-project session, selecting a file-backed custom theme stores only its file reference:

```toml
[theme]
file = "~/.config/blade/themes/company.toml"
```

The colors are read from that file whenever Blade starts, so they are not copied into `.blade` and do not mask later picker previews. A custom theme defined inline in the global configuration is referenced by its name because it has no source file.

## In-app picker

Press `T` in Blade to open the theme picker. Use `↑`/`↓` or `j`/`k` to preview each preset across the full interface. `Enter` saves the selected preset or custom-theme file reference to the project's `.blade`; in an **All projects** session it saves the selected name to the active global project list instead. `Esc` restores the theme that was active when the picker opened.

If the project does not have a `[theme]` table, Blade inserts one before the first `[[groups]]` table. Choosing a theme from the picker replaces the project's previous theme selection and color overrides while preserving the rest of the file. Manually configured color overrides remain supported when you want to customize a preset directly.

Color overrides are applied after the preset. Every field is optional:

| Field | Used for |
| --- | --- |
| `accent` | Focused borders, group labels, the logo, and help dialog |
| `accent_text` | Text inside the Blade logo |
| `muted` | Timestamps, inactive borders, system messages, and stopped commands |
| `text` | Normal command output |
| `footer` | Normal footer text |
| `search` | Search matches and input prompts |
| `waiting` | Waiting and pre-step states |
| `running` | Running commands |
| `stopping` | Stopping commands |
| `completed` | Successfully completed commands |
| `failed` | Failed commands |

Colors accept:

- Terminal names: `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`, `dark-gray`, `light-red`, `light-green`, `light-yellow`, `light-blue`, `light-magenta`, `light-cyan`, and `white`. Underscores and British `grey` spellings are accepted.
- True color as `#RRGGBB`, such as `#88c0d0`.
- An indexed terminal color as `ansi:0` through `ansi:255`.
- `reset`, `terminal`, or `default` to use the terminal's foreground color.

For example, a complete custom state palette can be layered on Gruvbox:

```toml
[theme]
preset = "gruvbox"
accent = "light-cyan"
accent_text = "black"
muted = "dark-gray"
text = "reset"
footer = "gray"
search = "light-yellow"
waiting = "yellow"
running = "light-green"
stopping = "light-yellow"
completed = "cyan"
failed = "light-red"
```

Invalid preset names or colors are validation errors. Unknown fields in `[theme]` are warnings. True-color values depend on the terminal's color support.
