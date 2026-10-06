# Sessions, workspaces, tabs and panes

A **session** is one server (one control socket, one state database). Inside it, **workspaces** group related work (usually one checkout); each workspace has **tabs**, and each tab a tree of **panes** (splits, plus floating panes). Panes are addressed by handle: `w1`, `w1:t2`, `w1:p3`.

Several clients (TUI, CLI, gateway, plugins) can be attached at once. The TUI that is actively resizing holds the geometry lease. The sidebar shows execution state and attention markers per workspace, with a needs-you section on top.

Everything the TUI does is a control-API call, so everything is scriptable: `vibeke workspace|tab|pane ...` (see the [CLI reference](../reference/cli.md)).
