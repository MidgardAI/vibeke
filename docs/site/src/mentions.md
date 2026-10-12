# Mention other agents

You can give a pane a short name, such as `@reviewer`. Then you, or an agent in another pane, can send a message to that name. The reply comes back to the pane that asked.

Messages stay in one workspace on one machine. Claude Code and Codex agents can receive messages.

## Name a pane

Only a named pane can receive messages. Naming a pane means you allow the other panes of the workspace to message it.

- In the terminal interface, press **Ctrl+B**, then **@**. The picker lists the other panes of the workspace. A pane without a name shows a suggested name. Press **Enter** to name the pane and insert `@name` into your pane.
- Open the command palette (**Ctrl+B**, then **:**) and choose **Name this pane** or **Remove this pane's name**.
- In a pane, run `vibeke name @reviewer`. Without a name, Vibeke suggests one from the pane label and prints it.
- In the web app and desktop app, type `@` in the message box. Choose a pane from the list. A pane without a name gets its suggested name first.

A name has 1 to 32 characters: lowercase letters, digits, `-` and `_`. It starts with a letter. Words such as `current`, `focused` and `all` are not allowed. Each name belongs to one pane in a workspace. The name stays when the agent in the pane restarts.

Run `vibeke name --clear` to remove the name of the current pane. Use `--pane <target>` to name another pane yourself.

## Send a message

Start a prompt in an agent pane with a name. Vibeke sends the rest of the prompt to that pane:

```text
@reviewer please check the changes in src/auth.rs
```

You can use several names at the start: `@reviewer @tester run the new tests`. If any name is unknown, Vibeke sends no message. Your prompt goes to the agent as usual, with a note that lists the known names.

From a script or a shell, use the CLI:

```sh
vibeke msg @reviewer "please check the changes in src/auth.rs"
vibeke msg @reviewer @tester "run the new tests"
echo "long text" | vibeke msg @reviewer -
```

The text `-` reads the message from standard input. Add `--no-reply` when you do not need an answer.

## How a message arrives

A busy agent gets the message at the next chance. The integration hook of the agent adds the message when you send the agent a prompt, or when its turn ends. Claude Code and Codex also get it after a tool call. The agent does not have to stop first. Vibeke never types into the prompt while you write in it.

An idle agent gets the message right away. Vibeke types one short line into the prompt of that pane. The line has the message id, the sender and the first words of the message. The integration hook of the agent replaces this line with the full message.

In both cases, the agent sees who wrote the message, how to reply, and the list of named panes.

The receiving agent can answer with a reply. The reply goes to the pane that sent the message. The sender sees it as a new message. If the agent stops without a reply, it gets one reminder.

To use messages, install the agent integration. Run `vibeke integration install claude` or `vibeke integration install codex`. To let the agent send and reply on its own, also run `vibeke integration install claude --mcp`. This adds the Vibeke tools to the agent. Run `vibeke doctor` to see what is missing.

## Answer and read messages

Use these commands in a pane:

```sh
vibeke msg inbox             # messages sent to this pane
vibeke msg list              # messages sent and received
vibeke msg reply m_123 "Looks good, one nit in line 40."
vibeke msg done m_123        # finish without a reply
```

Add `--json` to get JSON output.

## Tools for agents

`vibeke mcp` gives agents these tools:

| Tool | What it does |
| --- | --- |
| `mentions_roster` | Lists the named panes of the workspace and their state. |
| `message_send` | Sends a message to one or more names. |
| `message_reply` | Answers a message by its id. |
| `message_complete` | Marks a message as handled without a reply. |
| `message_inbox` | Lists the messages sent to this pane. |

## Safety

- Messages stay in the workspace of the sender. A pane cannot message another workspace.
- Only named panes receive messages. An agent can name only its own pane. Only a person can name other panes.
- The text of a message is not trusted. Vibeke tells the receiving agent that the text comes from another agent and gives no permissions. A message cannot approve a request for the receiving agent.
- Vibeke removes secrets from message text before it saves it.
- A message can be 64 KiB at most. A pane can send 20 messages a minute. A chain of replies stops after 8 steps.
- If a pane has no agent, or the agent has no hook, the message waits and then stops. Only a person can send it again.
