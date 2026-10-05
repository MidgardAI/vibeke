# 14 — LLM assistance with genai

**Decision:** use the Rust [`genai` crate](https://github.com/jeremychone/rust-genai) for Vibeke's own LLM features, behind a small `vk-llm` module. Users choose their provider and model. Start with one backend implementation.

**Status:** design accepted for the provider-library choice; the interfaces and defaults below are proposed and require implementation and validation. This file does not claim that AI assistance is available today. It is an optional workstream, not a prerequisite for [milestones](11-milestones.md) or an expansion of its scope. The rollout stages here are separate from the Phase 1 milestones in [11](11-milestones.md).

Related: [01 architecture](01-architecture.md), [02 state and events](02-data-model-and-event-log.md), [07 API](07-api-cli-plugins.md), [08 UX and configuration](08-ux-config-and-keybindings.md), [09 security and privacy](09-security-and-privacy.md), [12 Phase 2](12-phase-2-outlook.md).

[15 — Task outcomes, review and attention](15-task-outcomes-review-and-attention.md) specifies the task/inbox surfaces that can consume intent suggestions, explanations and review prose from this module. Those surfaces also work without an LLM; generated drafts do not confirm intent, send messages, authorize checks or accept work.

## 1. Product purpose

Help the user understand, find and supervise work across coding agents. The assistant uses Vibeke's structured state and selected transcript material to reduce the time spent switching panes and reconstructing context.

The model selected here powers **Vibeke's assistant**. It does not select or change the models inside Claude Code, Codex, pi, omp or another hosted harness. A user can run Codex in a pane and use a local model for Vibeke's summaries.

The first useful feature is an on-demand briefing: **"What needs me?"** A response might say:

> samplehub is waiting for your answer about the migration. dashboard's task finished, but its recorded browser check failed. Two other agents are still working.

Each item links to the corresponding run, interaction or check. The user can inspect the source, focus the pane or open the existing interaction card.

## 2. Features and delivery order

| Feature | User experience | Required context | Delivery |
|---|---|---|---|
| **Briefings** | "What needs me?", "What changed since lunch?" | State snapshot, open interactions, recent turns and recorded outcomes | First feature |
| **Semantic navigation** | "Find the agent fixing login" or "Find our discussion about refunds" | Authorized task/run metadata and bounded search results | Next |
| **Contextual decision cards** | Explain why an agent is asking, cite earlier decisions, draft an editable reply | Exact open interaction and selected surrounding turns | Next |
| **Possible-stall notices** | "The same installation failed four times; inspect registry authentication" | Repetition signals, tool errors and task objective | Later, opt-in background feature |
| **Review briefs** | Summarize the change, recorded validation and work still outstanding | Original request, diff, revision-bound `EvidenceRecord`s | Later; depends on evidence groundwork in 12 |
| **Handoffs** | Prepare objective, decisions, attempts and remaining work for another agent | Selected task history and exact workspace revision | Later; user reviews and sends through existing harness APIs |

Automatic task titles can follow the briefing slice. Preserve user-assigned names; generated suggestions are editable. Goal decomposition, autonomous agent scheduling, automatic replies/approvals, automatic merge and learned permission rules remain outside this specification's initial delivery.

## 3. Why genai and what we reuse

`genai` supplies a common Rust client across providers, including streaming, tool definitions, structured-output options and custom endpoint/auth resolution. Its documentation covers native protocols as well as compatible endpoints and local Ollama. Provider/model capabilities still differ. [Upstream README](https://github.com/jeremychone/rust-genai)

This fits Vibeke's Rust and single-binary architecture. We reuse its adapters and transport handling rather than maintaining a provider SDK per vendor. Pi's JavaScript provider helper and Rig are not part of the initial implementation.

| Owned by genai | Owned by Vibeke |
|---|---|
| Provider request/response conversion and stream decoding | User settings, explicit endpoint/model selection and credential references |
| Provider-specific wire formats and supported generation options | Context selection, provenance, redaction and output validation |
| Available provider/model lookup facilities | Model picker, capability records and clear unsupported-feature states |
| Provider errors and reported usage | Request lifecycle, budgets, cancellation, caching and UI |

Pin a released crate version during the integration spike and record the resolved version in `Cargo.lock`. Verify that version's API and release notes; upstream `main` documentation may describe unreleased behavior. Updates go through the same contract tests as the first integration. No dependency is added as part of this specification.

## 4. Architecture and execution host

```text
TUI / CLI / future web client
            |
      assistant.* API
            |
      vk-server request coordinator
       |                  |
authorized context    vk-llm -> genai -> selected provider
       |                  |
state / events       validated result + source references
```

- **`vk-llm`** owns the small generation interface and the `genai` adapter. Provider crate types do not appear in `vk-proto`, stored records or feature implementations. It has no direct access to PTYs, holder sockets, shell execution or permission changes.
- **The server coordinator** checks caller scope, prepares context, resolves the profile, enforces limits and records lifecycle changes through the existing state actor/outbox. Network calls run on bounded asynchronous tasks, outside the state actor and terminal render/input path.
- **Feature code** constructs a versioned prompt and an output schema. Start with one briefing operation; avoid building a general agent framework before a feature needs it.
- **Clients** render progress, validated results and source links. Streaming deltas are transient; the final validated result and lifecycle are durable. A slow/disconnected client cannot block the provider reader or terminal tasks.

The **coordinator's machine** owns credentials, makes the provider request and interprets endpoint addresses. With a local Vibeke server connected to a devbox, the local server can gather authorized remote context and call the provider locally. With plain `ssh devbox` followed by Vibeke there, the devbox is the coordinator: credentials and `localhost` endpoints refer to that machine. Display this in setup and diagnostics. No automatic credential copying or fallback to a different machine.

Cross-machine requests use explicit machine/session identities and a cursor per source session. A remote server supplies scoped source data, not instructions to call a provider or choose credentials. An offline source is marked unavailable or stale; the briefing states its coverage.

## 5. Provider and model selection

### 5.1 Setup

AI assistance is disabled by default. The user enables it, chooses a provider connection, supplies an API key reference or local endpoint, and selects a model. Setup identifies where the request runs and which workspace content may be sent to that provider. A connection test is an explicit small generation and is counted as usage.

Initial integration targets are OpenAI, Anthropic, Gemini, OpenRouter, local Ollama and a custom OpenAI-compatible endpoint. These are **validation targets**, not a promise that every model or capability works. Other adapters can be exposed once verified against the pinned crate version.

A connection has a stable user-defined ID and an explicit protocol adapter, endpoint and credential reference. A profile binds that connection to an exact model ID and generation limits. Resolve by these explicit fields; do not let an unfamiliar model name silently select another adapter, endpoint or local model.

### 5.2 Model picker

- Offer models from the configured provider's supported listing facility where available. Label whether a list is live, cached or bundled, and show when it was refreshed.
- Always allow an explicit model ID. Listing may be unavailable or incomplete; do not require a central Vibeke model registry to make a request.
- Treat model existence/access and capabilities separately. A listed model may be unavailable to this credential. A manually entered model begins with unknown capabilities.
- Record support for text, streaming, JSON-schema output, tools and images as `supported | unsupported | unknown`, with its source and verification date. Native structured output is helpful, but every result is validated by Vibeke.
- Show provider/model and execution machine on a result. A model change affects new requests; in-flight requests retain their resolved profile.
- Provider failure does not switch models or providers automatically. The user can change a profile or retry explicitly.

### 5.3 Profiles

Start with one default profile. Optional `background` and `review` profiles let the user select a smaller model for summaries and another for detailed review. Missing feature-specific profiles use the default **only if it meets that feature's requirements**. Explicitly misconfigured profiles fail visibly rather than falling back.

A local-only workspace can use only a user-approved local connection. Provider selection for a workspace is configured in the user's settings; repository content cannot redirect assistant traffic.

## 6. Proposed configuration

This is a **proposed extension**, not currently supported configuration. When implemented, its Rust types and generated reference must be added to the canonical schema in [08 §11](08-ux-config-and-keybindings.md). That reference remains authoritative for shipped settings.

```toml
[assistant]
enabled = false
default_profile = "interactive"
background_enabled = false
max_concurrent_requests = 2
max_queued_requests = 16
daily_request_limit = 100
request_timeout_seconds = 60
result_retention_hours = 24

[assistant.connections.primary]
adapter = "anthropic"
credential = { env = "VIBEKE_ASSISTANT_API_KEY" }
# Alternatively: credential = { keychain = "vibeke/assistant/primary" }
# The adapter's standard endpoint is used unless endpoint is explicitly set.

[assistant.profiles.interactive]
connection = "primary"
model = "<selected-model-id>"
max_input_tokens = 12000
max_input_bytes = 65536
max_output_tokens = 1024
```

The model ID above is a placeholder, not a default model recommendation. No provider is contacted until assistance is enabled and a complete profile is selected. In v1, these settings are user-level only; repository-local `[assistant]` settings are rejected with an explanation. Per-workspace consent records are also user-owned and bind a canonical workspace identity to allowed connection IDs and context classes. Endpoint/adapter changes invalidate the corresponding grant.

Credentials are resolved on the coordinator. Use OS credential storage where available, or an explicitly named environment variable on headless machines. Credential references are exclusive: failure to resolve one does not fall back to ambient provider credentials. No plaintext key in TOML, SQLite, argv, logs or RPC results. Keys retrieved for the assistant are never injected into agent environments. This does not alter credentials the user separately configures for a harness. Subscription/OAuth login is deferred; adapter availability alone does not establish support for an existing subscription.

## 7. Context and evidence

### 7.1 Context construction

The first feature assembles its context deterministically; the LLM receives no retrieval tools. The coordinator selects:

1. The explicit scope (workspace, task, run or selected set), requested time window and current structured snapshot.
2. Open interactions, their current delivery/status fields and recent relevant turns.
3. Bounded transcript excerpts or tool outcomes required to explain an item.
4. For later review features, diffs and revision-bound evidence.

Apply caller authorization and workspace/provider consent **before** retrieving content, cache lookup or sending it. Session-wide briefings include only permitted workspaces and report exclusions. Output and source references inherit the same access scope. Recheck access when a stored result is read.

Prefer structured adapter information. Screen-only excerpts are opt-in and labeled inferred. Never dump all scrollback, the whole repository, credential files or environment values into a prompt. Files, screenshots and full diffs require their own context class; the initial briefing is text-only. Selected excerpts are run through `vk-redact` before transmission, while preserving an explicit notice that pattern redaction cannot guarantee removal of all sensitive content.

The context builder assigns source IDs; each contains machine/session/object identity, cursor or turn/item reference, observed time and a digest of the included content. These IDs are the only source references a generated answer may use. The request records omitted/truncated categories and any unavailable sources. A history gap is not described as a complete account of the requested period.

### 7.2 Grounded output

The briefing result contains short items with text, existing target IDs and source IDs, plus coverage notes. The model must distinguish observed facts, agent claims and suggestions. Schema validation rejects invented IDs and invalid targets; this verifies structure and reference membership, not semantic truth. Evaluation must separately check whether cited sources support each claim.

Execution state, open approvals, delivery state and test results remain authoritative in Vibeke. Generated text cannot modify them. "The agent says tests passed" is distinct from a recorded successful check. Review briefs use [12's evidence freshness rules](12-phase-2-outlook.md); a screenshot is described with its recorded environment.

For tracked-task review briefs, [15 §6–7](15-task-outcomes-review-and-attention.md) refines the evidence categories and freshness rules: an observed command can have passed while its code binding remains unverified and its criterion remains unsupported. Generated prose must preserve that distinction.

The UI labels generated interpretation and shows its timestamp and sources. If relevant state has changed, mark the result stale and offer refresh. Revalidate the live target when the user opens or acts on an item; never act on an interaction's cached status.

## 8. Requests, validation and persistence

The internal boundary uses Vibeke-owned types, with this conceptual shape (not a committed Rust API):

```text
LlmRequest {
  request_id, feature, resolved_profile, prompt_version,
  messages, output_schema?, limits, deadline
}
LlmEvent = TextDelta | Usage | Finished | Failed
AssistantResult {
  request_id, feature, scope, provider_connection, model,
  created_at, source_refs, context_digest, prompt_version,
  output, coverage, usage?, estimated_cost?, finish_reason
}
```

Cancellation is an explicit handle tied to `request_id`, not text in a prompt. The coordinator owns source references and validates all output before publishing a completed result. Credentials are passed privately to the adapter and are not part of serialized request records.

Persist `queued -> running -> completed | failed | cancelled | interrupted` with timestamps and sanitized error categories. On restart, unfinished requests become `interrupted`; do not replay a paid request automatically. Disconnecting a client does not create another request. A caller-scoped idempotency key deduplicates submission/reconnect, while explicit Retry creates a new request with a link to the earlier one. Coalesced consumers cannot cancel another client's request.

Limits and behavior:

- Bound context by both bytes and estimated tokens, including the system prompt and output schema. Reserve room for output within the model context window. Record the estimation method; unknown context limits require a configured limit or a verified capability record. Estimates are not provider billing counts.
- Apply one total deadline across queueing and network activity. On timeout/cancel, abort the stream and stop publishing deltas. Already consumed provider usage may still be billed; unknown usage remains unknown.
- Use a finite queue and prioritize interactive work. Background work is coalesced per task and cannot consume every concurrency slot. With a concurrency limit of one, it yields scheduling priority but does not preempt an already running request.
- Native schema mode is used only when supported. Otherwise request JSON text and validate it locally. Permit at most one bounded repair attempt, counted against limits; invalid, refused or truncated output never becomes a completed briefing.
- Retry only explicit retryable failures such as a rate-limit response before content, at most once within the deadline and respecting `Retry-After`. Do not automatically retry ambiguous connection failures after submission or a partially delivered stream.
- Each provider attempt, including connection tests, retries and repairs, counts toward the request limit. Reserve allowance atomically before dispatch. The limit is per coordinator session and UTC day; it is not a shared account quota across machines/sessions.
- Display reported token usage and separately labeled cost estimates when pricing is known. Missing pricing is `unknown`, not zero. Monetary hard caps are deferred; request and token bounds are the initial controls.

Retain completed output and metadata for 24 hours by default; raw assembled prompts are not stored again. Cache keys include requester access scope, workspace/provider grants, source content/version digests, feature/prompt/schema versions and the resolved profile. Expired grants or changed sources prevent a cache hit. `vibeke forget` for a source scope removes derived results and cached excerpts as well; purging a result also releases its blob references. Logs/outbox events contain lifecycle metadata and result IDs, not full prompts, generated text or provider response bodies.

Error categories include `disabled`, `not_configured`, `permission_denied`, `unsupported_capability`, `context_too_large`, `queue_full`, `budget_exhausted`, `authentication_failed`, `rate_limited`, `provider_unavailable`, `invalid_output`, `timeout`, `cancelled` and `interrupted`. Provider failures affect assistance only; terminal operation continues normally.

## 9. UX, API and action boundaries

Use the existing peek, inbox and user-invoked command surfaces. AI output never appears over a focused agent pane without the user's invocation and never takes keyboard focus. The briefing command can ship before the complete command palette. When disabled or unavailable, the normal structured attention list remains usable.

Proposed additions to [07](07-api-cli-plugins.md), exposed through the same JSON-RPC connection and CLI conventions:

| API | CLI | Purpose |
|---|---|---|
| `assistant.status` | `vibeke assistant status` | Enabled/configured state, coordinator, profile and remaining request allowance |
| `assistant.providers` | `vibeke assistant providers` | Configured connections and verified adapters; no secrets |
| `assistant.models` | `vibeke assistant models --connection <id>` | Model list and capability provenance; explicit refresh option |
| `assistant.generate` | `vibeke assistant brief --workspace <target>` | Submit scoped feature request; return request ID and cursor |
| `assistant.get` | `vibeke assistant get <request-id>` | Read authorized lifecycle/result; CLI can wait for completion |
| `assistant.cancel` | `vibeke assistant cancel <request-id>` | Cancel an owned request |

Full human clients may call these methods. Pane/adapter tokens receive no assistant access by default. Future delegation requires explicit read, generation/budget and cancellation scopes; it cannot use the assistant to widen source access. Model-list refresh also checks the connection and caller before making network requests. `vibeke doctor` diagnoses settings without generating paid requests.

Lifecycle events (`assistant.request_created`, `assistant.request_started`, `assistant.request_finished`) use the transactional outbox with `actor.kind = system` and the initiating client reference. Transient `assistant.delta` notifications carry request ID and sequence; they are not outbox history. Reconnecting clients recover the durable status/final result with `assistant.get`; incomplete streaming text is not promised to survive a restart. Delta subscriptions are authorized exactly like result reads.

The initial feature has no model-callable tools. Later navigation resolves suggestions to existing objects and asks the client to open them. Reply suggestions populate an editable draft; the user's Send action invokes the existing `interaction.answer` or `agent.prompt` path, with its live preconditions and delivery semantics. Preparing a handoff does not send it. Mutating command-palette actions, task creation and autonomous tool loops need a subsequent explicit design.

## 10. Privacy and failure isolation

Enabling cloud assistance introduces an intentional data-egress path beyond the default local storage in [09 §9](09-security-and-privacy.md). Setup must explain that distinction and persist the user's connection/workspace/context grants. Background analysis requires separate opt-in and follows the same grants. Disabling assistance cancels queued/running work and stops background generation and model refreshes; it cannot retract content already sent.

Repository text, transcripts, remote output and generated replies are untrusted content. They cannot choose endpoints, fetch secrets, grant permissions or add tools. Sanitize generated terminal output under the same control-character rules as other untrusted chrome text. Render model references as validated internal links; do not automatically open model-authored URLs.

Require HTTPS for non-loopback provider endpoints. Plain HTTP is supported for explicitly configured loopback services; a private remote endpoint should use TLS or a user-configured SSH tunnel. Do not follow cross-origin redirects with credentials. Honor user-configured transport/proxy policy and never disable certificate validation as a fallback. These constraints must be verified on the pinned client's actual HTTP path.

Credentials, content consent and cost limits apply on the coordinator even when all sources are sandboxed. No new ability is granted to a hosted agent. A stalled or failed LLM request cannot block holder recovery, input delivery, interaction answering or the server state actor.

## 11. Rollout and acceptance

### A0 — Integration spike

Use disposable fixtures and tiny opt-in live requests. Record the crate version, provider/model/endpoint and results. Validate:

- Static release packaging on Vibeke's macOS and Linux/musl targets, dependency/license checks and compatible TLS configuration.
- Text streaming, structured-result validation, explicit routing to a local/custom endpoint, usage/error mapping, cancellation and total deadlines.
- At least two native cloud adapters plus local Ollama/custom-compatible behavior. Publish a capability matrix; untested providers stay marked unverified.
- Tool-call argument decoding in a mock contract test for future use, without executing tools or requiring tools for the first feature.
- Credential resolution without ambient fallback, request redaction and prohibited redirect behavior.

If an upstream capability is missing, contribute a focused fix or defer that capability. Do not silently add a second backend or a collection of direct vendor clients. A backend change requires a new documented decision.

### A1 — On-demand briefing

Deliver setup/profile selection, the bounded coordinator, source-linked briefings, lifecycle/cancellation and normal offline/disabled behavior. Add the config/API/data types to their canonical specs as they are implemented.

Acceptance requires:

1. With assistance disabled, no provider calls occur and ordinary terminal workflows still pass their existing checks.
2. The user can switch between a verified cloud connection and a local one without changing any hosted agent's model or restarting its pane.
3. Every actionable briefing item resolves to authorized existing sources and live targets. Fixtures distinguish agent claims from measured checks and show stale/offline/gapped context correctly.
4. Tests cover malformed/refused/truncated output, unknown model capabilities, quota exhaustion, cancellation, restart and duplicate submissions without duplicate automatic generation.
5. Scope/grant tests cover excluded workspaces, revoked access, cache reuse, `forget`, untrusted endpoint instructions and prompt-injection attempts to answer approvals.
6. Concurrent slow streams do not violate [10's existing terminal latency budgets](10-quality-performance-testing.md); report measurements rather than assuming asynchronous code is sufficient.
7. A small evaluation set measures factual support, important blocked items omitted, incorrect urgency and time to find the needed interaction against the structured-list baseline. Record model/prompt versions and results before enabling background features.

### A2 — Navigation and decision assistance

Add semantic ranking over authorized search candidates and editable interaction reply drafts. Reuse the same provider/profile boundary and provenance model. Ordinary search and direct answering continue to work without an LLM.

### A3 — Background and Phase 2 features

Evaluate coalesced background summaries and possible-stall notices after A1 demonstrates useful output and acceptable cost. Review briefs depend on revision-bound evidence; handoffs depend on reliable task history. Mobile/web clients use the same API. This stage does not automatically authorize scheduling, policy learning, merging or agent-to-agent replies.

## 12. References and remaining verification

Upstream documentation reviewed 2026-10-06:

- [genai repository, features and examples](https://github.com/jeremychone/rust-genai).
- [genai crate documentation](https://docs.rs/genai) — implementation must use documentation for the pinned version.
- [genai release history](https://github.com/jeremychone/rust-genai/releases).

The provider-library decision is settled. The spike still needs to establish the exact crate release, transport/cancellation behavior, model-list provenance per adapter, static-build compatibility and the tested capability matrix. Prompt quality and the best default model remain empirical choices; this specification does not hardcode a commercial model or treat upstream documentation as end-to-end verification.
