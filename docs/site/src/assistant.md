# Host assistant

The host assistant uses a language model to help with small jobs. It is off by default. Vibeke sends nothing to a model provider until you turn it on, choose a provider and allow it for a workspace.

The assistant is a helper, not an agent. It cannot run commands, answer requests or change your files. It writes drafts. You read a draft, change it and decide what to do.

## What it does

- **Briefing.** It summarizes what happened in a workspace: open requests, running agents and tasks.
- **Task details.** It suggests a title, a goal and success criteria for a task from the turns that you select.
- **Review summary.** It summarizes the diff and the checks of a task.
- **Pane title and reply suggestions.** It proposes a short title for a pane or a short reply to an agent. The app sends nothing until you send it.
- **Navigation and decision cards.** It finds the pane, task or request that you describe. It explains an open request and drafts an answer. It never sends the answer.
- **Effort estimate and handoff notes.** It estimates how long a task may take and prepares notes for a handoff.

The assistant is experimental. Its provider connections have not been verified against every provider.

## Set it up

The assistant runs on the host that runs the server. That host keeps the credentials. Only your user configuration can turn it on. A repository cannot redirect assistant traffic.

1. Add a connection and a profile to `~/.config/vibeke/config.toml`:

   ```toml
   [assistant]
   enabled = true

   [assistant.connections.primary]
   adapter = "anthropic"   # anthropic, openai_compatible, ollama or gemini
   credential = { env = "VIBEKE_ASSISTANT_API_KEY" }

   [assistant.profiles.interactive]
   connection = "primary"
   model = "<model-id>"
   ```

   A credential can also come from a file (`{ file = "~/.config/vibeke/assistant.key" }`, mode 0600) or from the keychain. Vibeke never falls back to another credential that it finds on your computer.

2. List the models that the provider offers: `vibeke assistant models primary`.
3. Test the connection: `vibeke assistant test`. The test sends no workspace content. It counts as one request.
4. Allow the assistant for a workspace: `vibeke assistant consent <workspace>`.
5. Check the state: `vibeke assistant status`.

The [configuration reference](reference/config.md) lists every setting, including daily limits for requests, tokens and cost.

## Privacy: what is sent

Vibeke shows the exact text that it will send before it sends anything. Read it, then choose **Confirm and send**, or run `vibeke assistant confirm`. Vibeke sends exactly the text that you saw. If you do not confirm, the preview expires and Vibeke drops it.

The text goes only to the provider that you chose. The provider handles it under its own terms. Vibeke removes recognized secrets first, but it cannot find every secret.

You decide which kinds of content a workspace allows:

- selected text,
- structured state, such as task and request details,
- review packages, such as diffs and check results,
- screen contents. Vibeke includes the screen only when you allow it for the workspace and ask for it.

Consent belongs to one workspace and one connection. If you change the provider address, consent ends. If you revoke consent with `vibeke assistant revoke <workspace>`, Vibeke cancels the unfinished requests there.

To skip the preview for an operation, list it in `auto_send`. The workspace consent must list it too. Vibeke never skips the preview for a selection that spans several workspaces.

Background features are off by default. Without `background_enabled`, no request starts because a turn ended.

Vibeke keeps results for 24 hours. Run `vibeke assistant purge --all` to delete them at once. Audit records hold metadata only, not the text.
