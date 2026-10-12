# Script and automate Vibeke

You can control Vibeke from shell scripts, Python or TypeScript.
Everything the terminal interface does goes through one local control API, so a script can do the same work.

## Use the CLI

Each command has the form `vibeke <noun> <verb> [positionals] [--flag value]`.
The noun is the API namespace. The verb is the method name. For example, `vibeke agent list` calls `agent.list`.
The [CLI reference](reference/cli.md) lists every command and the method that it calls.

```sh
vibeke workspace list
vibeke agent list --json
```

Use `--json` to print JSON. When the output is not a terminal, Vibeke prints JSON without the flag.
Add `--session NAME` to select a session, `--socket PATH` to select a socket, and `--timeout MS` to limit the wait.

### Call any method

Use `vibeke api call` to call a method that has no command. Pass the parameters as a JSON object.

```sh
vibeke api call server.status
vibeke api call agent.list '{"harness":"claude"}'
vibeke api call api.schema '{"method":"agent.start"}'
```

The [API reference](reference/api.md) lists all methods and the schema of their parameters and results.
Run `vibeke api methods` to list the methods of the server you are connected to.

### Wait for events

Two commands read events:

- `vibeke events read [--after-seq n] [--types agent.*]` prints past events.
- `vibeke events wait --types agent.state_changed --timeout-ms 60000` waits for the next matching event and prints it.

To wait for one agent, use `vibeke agent wait <target> --until idle,done,needs_approval`.
For a continuous stream of events, use one of the clients below.

## Access and scope

The CLI and the clients connect to the server through a Unix socket in a private directory of your user.
Clients refuse a socket that another user could have created. Other local users cannot connect.

A program that runs outside a pane has full access.
A program that runs in a pane can use the pane token in the `VIBEKE_PANE_TOKEN` environment variable.
The token limits the connection to pane scope. It can read state and act on its own panes.
It cannot approve requests, change policy or stop the server.
The [API reference](reference/api.md) shows the access level of each method.
The [security page](security.md) explains why pane scope is a guardrail and not a security boundary.

## Use the Python client

The Python client uses only the standard library and needs Python 3.11 or later.
The package is called `vibeke-client`. It is not on a package index yet. Use it from a checkout of the repository:

```sh
git clone https://github.com/vibeke-ai/vibeke
pip install ./vibeke/clients/python
```

This example lists the agents that wait for you, then waits until all other agents are idle.

```python
import asyncio
from vibeke_client import Client

async def main():
    async with await Client.connect() as c:
        open_requests = (await c.interaction_list({"status": "open"}))["interactions"]
        for item in open_requests:
            print("needs you:", item["run"], item["title"])

        runs = (await c.agent_list({}))["runs"]
        working = [r for r in runs if r["execution"]["value"] == "working"]
        for run in working:
            result = await c.agent_wait({
                "target": run["id"],
                "until": ["idle", "needs_approval", "needs_answer", "error", "exited"],
                "timeout_ms": 600000,
            })
            print(run["handle"], "->", result["state"])

asyncio.run(main())
```

`Client.connect()` reads `$VIBEKE_SOCKET`, or finds the socket of the default session.
To react to changes as they happen, subscribe to events:

```python
async with await Client.connect() as c:
    events = await c.events(types=["agent.*"])
    async for ev in events:
        print(ev.type, ev.data)
```

Errors raise `VibekeError`. The method `c.call("method.name", {...})` calls any method without types.

## Use the TypeScript client

The TypeScript client is called `@vibeke/client`. It has no runtime dependencies and needs Node 22.18 or later, or Bun.
It is not published to a package registry. Use it from a checkout of the repository by importing `clients/typescript/src/index.ts`, or by copying the `clients/typescript` folder into your project.

This example lists the agents with their states, and then follows state changes.

```ts
import { VibekeClient, isEvent } from "@vibeke/client";

const c = await VibekeClient.connect();

const { runs } = await c.call("agent.list", {});
for (const run of runs) {
  console.log(run.handle, run.execution.value);
}

const events = await c.events({ types: ["agent.state_changed"] });
for await (const e of events) {
  if (isEvent(e, "agent.state_changed")) {
    console.log(e.subject.run, "is now", e.data.to);
  }
}
```

Errors reject with `VibekeError`. If the server drops events, the stream ends with `EventOverflow`. Subscribe again with `{ after: err.resumeFrom }`.

## Add custom commands

You can add your own key bindings to the terminal interface in `config.toml`.
A binding can run a shell command, or open a pane, popup or float.

```toml
[[keys.command]]
key     = "prefix+alt+g"
type    = "popup"
command = "lazygit"
width   = "80%"
height  = "80%"
```

The `type` is `shell`, `pane`, `popup` or `float`. Other fields are `cwd`, `env`, `title` and `when`.
See the [configuration reference](reference/config.md).

## Next steps

- [API reference](reference/api.md): see every method, its parameters and its access level.
- [CLI reference](reference/cli.md): find the command for each method.
- [Security](security.md): learn what pane tokens and scopes protect.
