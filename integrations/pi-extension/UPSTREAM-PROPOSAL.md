# Proposal for pi: make observing (and resolving) extension UI dialogs an official API

*Optional. Vibeke already works without this: in TUI mode `@vibeke/pi-extension` wraps the shared `uiContext` (DESIGN.md §4.1), which relies on pi internals (`ctx.ui` returning one shared, mutable object; `signal` dismissing dialogs). This proposal asks pi to make that supported, so integrations stop depending on internals.*

*Draft for the pi maintainers. Target: `@earendil-works/pi-coding-agent` extension API. Written by the Vibeke project; applies equally to any multiplexer, dashboard or remote client.*

## Motivation

pi leaves permissions to extensions by design: a permission extension calls `ctx.ui.confirm()` / `ctx.ui.select()` from a `tool_call` handler and blocks until the user answers. That works well at the terminal.

Tools that supervise several agents (terminal multiplexers, phone clients) need to know **that** a dialog is open and **what** it asks, so they can tell the user "pi in pane 4 is waiting for you". Today:

- In **RPC mode** this already works: dialogs are emitted as `extension_ui_request` and answered with `extension_ui_response` (`rpc.md` §Extension UI Protocol).
- In **TUI mode** there is no supported way for another extension to see them. Today an integration can either screen-scrape pi's dialog widget (fragile) or wrap the shared `uiContext` methods (works in 0.84, but depends on `ctx.ui` being one shared mutable object and could break on any refactor).

The RPC protocol shows the shape is already right. The proposal brings the same information to extensions in TUI mode, without changing how dialogs look or behave.

## Proposed API

Two observer events, emitted for every dialog method (`select`, `confirm`, `input`, `editor`) in every mode:

```ts
pi.on("ui_request", (event: UiRequestEvent, ctx) => void);
pi.on("ui_response", (event: UiResponseEvent, ctx) => void);

interface UiRequestEvent {
  id: string;                         // same id space as RPC extension_ui_request
  method: "select" | "confirm" | "input" | "editor";
  title: string;
  message?: string;                   // confirm
  options?: string[];                 // select
  placeholder?: string;               // input
  prefill?: string;                   // editor
  timeout?: number;
  source: { extension: string };      // which extension opened the dialog
  toolCallId?: string;                // set when opened during a tool_call handler for that call
}

interface UiResponseEvent {
  id: string;
  outcome: "value" | "confirmed" | "cancelled" | "timeout";
  value?: string;                     // select/input/editor
  confirmed?: boolean;                // confirm
  resolvedBy: "user" | "extension" | "timeout" | "rpc";
}
```

Optional, separately reviewable:

```ts
ctx.ui.resolve(id: string, response: { value?: string; confirmed?: boolean; cancelled?: boolean }): boolean;
```

`resolve` lets an extension answer an open dialog on the user's behalf, exactly as an RPC client's `extension_ui_response` does today. It returns `false` if the dialog is already closed. The TUI closes the dialog as if the user had answered, and `ui_response` reports `resolvedBy: "extension"`.

## Semantics

- Observer events are fire-and-forget, so handlers cannot delay or change the dialog. Exceptions in handlers are caught and logged like other events.
- `ui_request` fires before the dialog is shown; `ui_response` fires exactly once per request.
- `source.extension` lets observers label the prompt ("permission-guard asks…") and lets users filter.
- `toolCallId` links a permission prompt to the tool call it guards, so supervisors can show the command or diff.
- Fire-and-forget UI methods (`notify`, `setStatus`, …) are out of scope; they could be added later with the same pattern.

## Backward compatibility

- Purely additive: extensions that don't subscribe see no change, and dialogs render identically.
- RPC mode keeps its existing protocol. The new events fire there too, so one extension can behave the same in both modes.
- `resolve` is opt-in per call, and pi could gate it behind a setting (`allowExtensionUiResolve`) if the maintainers prefer.

## Why not keep wrapping `uiContext`?

It works today, but it monkey-patches a shared object, can't see which extension opened a dialog or which tool call it guards, and silently breaks if pi changes how `ctx.ui` is provided. Screen detection is worse still. The data already exists inside pi (the RPC path proves it); exposing it to extensions keeps pi's "everything through extensions" model and benefits any tool that integrates with pi, not just Vibeke.
