# Customize keys and appearance

This guide shows how to change keys, colors, and layout options in the configuration file.

## Find and edit the configuration file

1. Run `vibeke config path` to print the file location. By default it is `~/.config/vibeke/config.toml`. Set `VIBEKE_CONFIG` to use another file.
2. Run `vibeke config edit`. This creates the file from the default template if it does not exist, and opens it in your editor.
3. Run `vibeke config validate` to check the file.

Vibeke reloads the file when it changes (`[config] watch = true`). If the file has an error, Vibeke keeps the previous settings. To reload at once, press **Ctrl+B**, then **Shift+R**, or run `vibeke config reload`.

Run `vibeke --default-config` to see every setting with its default. See the [configuration reference](reference/config.md).

## Change the prefix key

The prefix key is **Ctrl+B**. To use **Ctrl+A** instead:

```toml
[keys]
prefix = "ctrl+a"
```

Other prefix settings:

```toml
[keys]
prefix_timeout_ms = 1500   # how long a second key can follow the prefix
prefix_menu = true         # show the key menu after the prefix
prefix_menu_ms = 400       # wait before the menu opens; 0 opens it at once
```

## Rebind a key

Each action has a name and a binding. A binding with `prefix+` needs the prefix first. A binding without it works on its own. An empty string removes the binding.

```toml
[keys]
new_tab = "prefix+t"
detach  = ""
```

1. Run `vibeke keys` to list the active bindings.
2. Edit the file.
3. Run `vibeke keys check`. It prints each pair of actions that share a binding and exits with an error if there are any.

A range such as `prefix+1..9` binds nine keys. To go back to the defaults, run `vibeke config reset-keys --dry-run` to see what would be removed. Then run it again without `--dry-run`. Vibeke saves the previous file first.

## Add your own commands

A `[[keys.command]]` entry binds a key to a command. The `type` is `shell`, `pane`, `popup`, or `float`.

```toml
[[keys.command]]
key     = "prefix+alt+g"
type    = "popup"
command = "lazygit"
width   = "80%"
height  = "80%"
```

You can also set `cwd`, `env`, `title`, and `when` (for example `when = "agent:claude"`). A plugin action uses `type = "plugin_action"` and `command = "<plugin>.<action>"`.

## Use copy mode

Press **Ctrl+B**, then **[** to enter copy mode. Move with the vi keys (`h`, `j`, `k`, `l`), start a selection with `v`, and press `y` or Enter to copy. Press `q` to leave. Press `/` to search forward, `?` to search backward, and `n` or `N` to repeat. Press `o` to select the output of one command.

Press **Ctrl+B**, then **/** to search the scrollback.

Choose emacs keys or change single keys:

```toml
[keys.copy_mode]
mode = "emacs"          # vi | emacs
"ctrl+e" = "edit_scrollback"
```

An empty string unbinds a key. The action names are in the default template.

## Choose a theme

```toml
[theme]
mode = "auto"           # auto | light | dark
dark_name  = "tokyo-night"
light_name = "catppuccin-latte"
```

With `mode = "auto"`, Vibeke follows the light or dark setting of your terminal. It uses `dark_name` in dark mode and `light_name` in light mode. Set `mode = "dark"` or `mode = "light"` to force one. The built-in names are `catppuccin`, `catppuccin-latte`, `terminal`, `tokyo-night`, `dracula`, `nord`, `gruvbox`, `one-dark`, `solarized`, `kanagawa`, `rose-pine`, and `vesper`.

Override single colors:

```toml
[theme.custom]
accent = "#f5c2e7"
```

## Change the sidebar, tabs, and status bar

```toml
[ui.sidebar]
position = "right"      # left | right
width    = 32
collapsed = false

[ui.tabs]
position     = "bottom" # top | bottom | hidden
show_numbers = false

[ui.status_bar]
enabled = true
position = "bottom"
left   = ["machine", "task", "branch"]
center = ["attention"]
right  = ["agents_summary", "clock"]
```

The status bar is off by default. The available items are `machine`, `session`, `workspace`, `task`, `branch`, `ports`, `attention`, `agents_summary`, `cpu`, `clock`, `prefix_indicator`, `mode`, and `sync_input`. Press **Ctrl+B**, then **b** to show or hide the sidebar.

## Change notifications

```toml
[notifications]
channel = "native"          # native | osc | both | none
sound   = "none"
quiet_hours = "22:00-07:00"

[notifications.on]
done = false
needs_approval = true
```

Vibeke does not notify you about the pane you are looking at (`suppress_when_focused = true`). Notifications for events close together are combined for `coalesce_ms` milliseconds.

## Next steps

- [Using the terminal interface](terminal.md): learn the default keys.
- [Configuration reference](reference/config.md): see every setting.
- [Run agents in parallel](parallel-agents.md): use tasks and synchronized input.
