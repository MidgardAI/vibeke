# Sessions, workspaces, tabs, and panes

A **session** has one server, one control socket, and one state database.

A **workspace** groups related work, usually for one repository checkout. Each workspace contains **tabs**. Each tab contains **panes** in a split layout. A tab can also contain floating panes.

Handles identify each object. For example, `w1` identifies a workspace, `w1:t2` identifies a tab, and `w1:p3` identifies a pane.

## Clients

Terminal clients, CLI clients, gateways, and plugins can connect to the same session. The client that changes the terminal size controls the layout dimensions during that change.

The sidebar shows agent state and requests for each workspace. Requests that need an answer appear at the top.

Terminal actions use the control API. Scripts can use the same operations through `vibeke workspace|tab|pane ...`. See the [CLI reference](../reference/cli.md).
