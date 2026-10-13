# 20 — Share links: design review, continue a session, unattended sharing

Status: phase 1 in progress.

## 1. Goal

Three ways to send work to another person with one link:

1. **Design review.** A developer shares one preview. A designer opens the link in their own
   browser, uses the running app, picks elements or areas, writes notes, and sends them. The
   notes reach the agent that owns the preview. The designer sees the next version at the same
   link.
2. **Continue this session.** An agent's session (branch, uncommitted changes, conversation)
   becomes a link, for example in a pull request. A teammate opens the link and continues the
   session on their own host. The sender's host may be offline by then.
3. **Unattended sharing.** A program on the host, such as a Hermes bot on Slack, starts an agent,
   declares a preview and creates the links above without a person at the terminal. Links it
   posts in a channel work for several teammates, so they are tied to GitHub accounts, not to
   whoever clicks first.

Every link opens in the browser app (PWA, including the WASM terminal view) and the desktop app,
and can be pasted into the TUI (§7).

The people who receive these links are mostly colleagues. Opening a link is one click: no
fingerprint check and no confirmation on the host. The trust decision is made when the link is
created (who may open it, what it reaches, whether notes need the owner's approval). A host that
serves outsiders can run the agent and its preview in a sandbox (spec 13).

This builds on spec 16 §15 (live share, handoff, peers, `gateway.call`) and spec 06 §B3.5, §B3.6
and §B11 (desktop preview tunnel, phone preview screencast, annotations).

## 2. Shared pieces

### 2.1 Link kinds

All links keep their secrets in the URL fragment, so no web server sees them.

| Link | Form | Secret | Opens |
|---|---|---|---|
| Live share (existing) | `<app>/#/pair?d=<b64 PairingLink>` | psk + `pid:` ticket | browser app |
| Design review (new) | `<app>/#/review?d=<b64 PairingLink>` | psk + ticket | review screen (§3.6) |
| Continue (new) | `<app>/#/continue?d=<b64 ContinueLink>` | blob key `k` | choose one of your hosts |

`PairingLink.share` gains `access`, `uses`, `notes`, for team links `teams: [{id, name}]` (§2.5,
for display only) and, for review links, `preview {id, label}` and `sid` (§3.6).

**Old clients.** A client that does not know `access: github` sends `pair.claim` without an
attestation; the gateway refuses it with `upgrade_required` ("This link needs a newer Vibeke
app"). It never falls back to an unauthenticated claim. An old app that opens `#/review` or
`#/continue` shows its unknown-route screen. The PWA is always current; the desktop app shows
its update prompt.

### 2.2 Who may open a link: `access`

A share and a continue link carry `access`:

- `link`: whoever opens the link first gets the access. Single use, 15 minutes to open. This is
  today's behaviour.
- `github`: the opener signs in with GitHub through the account server. Only the listed GitHub
  accounts, members of the listed teams (§2.5), or with `["*"]` any GitHub account, may open it. The link can be used up to `uses`
  times (default 20, max 100) until the share expires. Each opener becomes a separate device
  with its own login shown in **People**.

`share.create` without `access` keeps today's behaviour (`link`) for kind `share`, so existing
callers do not change. Kind `review` requires `access`. The CLI verbs (§5.2) always send
`access`: `github` by default, `link` only with an explicit `--link`. A request for `github`
fails closed: it is refused, never turned into `link`, when there is no account server
(`unavailable`), no allow-list (`invalid_params`), or the host is not signed in
(`account_required`).

**Identities are GitHub user ids.** Logins can be renamed and reused, so the allow-list stores
ids. `share.create` and `handoff.link.create` take logins (`github: ["alice"]`) and resolve them
through the account server (`GET /v1/github/users/{login}` → `{id, login}`) once, when the link
is created. The share stores `[{id, login}]`; `login` is for display. Attestations and blob
downloads are checked by id.

`share.create` and `handoff.link.create` also take `teams: [team id | team name]` (§2.5), alone or
with `github`. `["*"]` stands alone: it may not be combined with `teams`.

The default list is the account's **sharing list**, kept on the account server and managed on
the account page (`<account server>/account`, §6.4) or through `/v1/sharing`: its people and the
teams ticked "share with by default". A request for `access: github` without `github` and without
`teams` uses both; when both are empty it is refused with `invalid_params` (fail closed). The CLI
and the TUI send a machine's `[sharing] github` config list instead when it has one (people only;
the account's teams are then not added):

```toml
[sharing]
github = ["alice", "bob"]   # this machine's list; empty: the account's sharing list
```

### 2.3 GitHub identity attestation

The account server proves a GitHub account to a host. The host learns the GitHub id and login,
nothing else.

1. The browser app creates the per-invitation static key (spec 16, `invite_static:<pid>`), on
   the app origin. It computes
   `nonce = b64url(sha256("vibeke-attest/1\0" ‖ pid ‖ "\0" ‖ device_static_pub))`.
2. It saves the pending pairing in session storage and sends the user to
   `<account>/v1/attest/start?aud=<relay host id>&nonce=<nonce>&return=<app origin>/`.
   The account server signs the user in with GitHub (OAuth web flow, its own state cookie on its
   own origin) and redirects to `<return>#/attest?t=<attestation>` (one fragment, built by the
   server; `#/attest?error=<code>` on failure). `return` must be one of the
   server's app origins (`--app-origin`, repeatable, plus `http://localhost:<port>` with
   `--dev`). Review origins (§3.6) are never return targets.
3. The attestation is `v1.<b64url json>.<b64url ed25519 sig>`, signed over
   `"vibeke-cloud/1 attest\0" ‖ json`. The JSON is
   `{v:1, kid, gh_id, login, aud, nonce, iat, exp, teams?}` with `exp = iat + 300`. `kid` names
   the signing key (§6.1). `teams` lists the ids of the teams the signer belongs to when the
   account server signs (at most 50); it is left out when empty. A verifier refuses a token with
   more than 50 as malformed.
4. `pair.claim` carries `attest`. The gateway fetches the account server's keys
   (`GET <account>/v1/keys`, cached for 1 h, refetched at most once a minute when `kid` is
   unknown) and checks: the signature with the key `kid`; `aud` = its own relay host id;
   `nonce` = the value computed from the pid and the claiming device's static key; `exp` not
   past and `iat` not in the future (60 s skew); `gh_id` in the share's list, or one of the
   attestation's `teams` in the share's teams (§2.5), or the list is `["*"]`. On success the
   device gets `github: {id, login}`. A refusal is `github_not_allowed` with
   `details: {allowed: [login], teams: [team name]}`, so the app can say "This link is for:
   @alice and everyone in Acme".

The nonce binds the attestation to one invitation and one device key, so a captured attestation
is useless to another device. It travels inside the Noise channel.

**Desktop app.** The desktop app runs on `app://vibeke` and keeps pairing keys in its main
process, so it cannot run this browser round trip. It opens GitHub-gated pair and review links in
the system browser (the PWA) instead. Continue links need no attestation and work in the desktop
app.

Test vectors for attestations live in both repositories, like `host_claim_message`
(spec 16 §6.6).

### 2.4 Multi-use invitations

`Pairing` gains `uses: u32` and `redemptions: Vec<Redemption {device_static, device_id,
at}>`. It stays `Pending` until `redemptions.len() == uses` or the share expires. For
`access: github` the link's open window is the share's expiry, not 15 minutes.

`pair.claim` under the pairing lock:

1. If a redemption with the same `device_static` exists, it returns that device again
   (idempotent retry). It does not create a second device. The handshake admits an exhausted
   but unexpired, unrevoked invitation for this purpose only; a new device key is refused with
   `used`.
2. Otherwise it checks the count, creates the device, records the redemption, and writes the
   pairing, in that order. On start-up the gateway reconciles: a share device whose pairing
   lacks its redemption is added back. A redemption is never dropped: it counts toward `uses`
   even when its device is gone.
3. Revoking a device marks its redemption `revoked` (a tombstone). A retry with that device key
   is refused with `revoked`; the use is not given back.

Clients keep the invitation key after a transient failure (network, timeout, host offline) and
delete it only after a definitive refusal (`used`, `expired`, `revoked`, `forbidden`), so a
retry after a lost response is the same device.

Devices gain `invitation: Option<pid>`. **People** groups devices under their invitation.
`share.revoke {id: <pid>}` revokes the invitation and every device that came from it;
`share.revoke {id: <device>}` revokes one opener.

### 2.5 Teams

A team is a group of GitHub accounts on the account server. Everyone in a team may open the
GitHub links that a teammate makes for the team. A team has an id (a lowercase, hyphenated UUID)
and a name. Names are for display only; nothing is authorized by name. Members are `owner` or
`member`; the creator is the first owner, and a team keeps at least one owner. Owners invite
people by GitHub login (one use) or with an invite link (anyone who has it, until it expires after 7 days or is revoked), and remove members; a member may leave.

**Membership counts when a link is opened, not when it is made.** A person who joins later can
open links made before; a person who leaves or is removed cannot open them any more. For pairing
links the account server puts the opener's team ids in the attestation (§2.3), signed when the
opener signs in, so the gateway learns current membership without asking the account server. For
continue links the account server checks membership when the blob is fetched (§6.3).

Gateway side:

- `ShareSpec.teams: [{id, name}]` (absent in shares from older gateways; such shares behave as
  before). The claim check compares ids only.
- `share.create` and `handoff.link.create` resolve `teams` through `GET /v1/teams` of the host's
  account: an entry shaped like a team id matches that id; anything else matches a team name
  without case. A team the host's account is not in is `unknown_team` with `details.team`; a name
  several of its teams share is `invalid_params` (name the team by id). At most 50 teams per link.
  `teams` with `access: link` is `invalid_params`.
- The result of `share.create`, the `share.list` invitations and the link's `share` object gain
  `teams: [{id, name}]`. Continue links send the ids as the blob's `teams` (§6.3).
- `account.teams {}` → `{teams: [{id, name, role, members}]}` is a server-bridge method
  (`gateway.call`) for the TUI: owner only, refused to panes like the login methods.

Fail closed: a team link never becomes a bearer link. A client that asked for `teams` and gets a
result without `teams` (an older gateway that ignored the parameter) cancels the link
(`share.revoke`) and reports that the gateway needs an update. An account server without the team
routes answers 404; `GET /v1/teams` then counts as no teams, so naming a team fails with
`unknown_team`.

## 3. Design review

### 3.1 Flow

1. The developer, or an agent's user, creates a review share for one preview:
   `vibeke share review v4` (GitHub by default) or the TUI People form (§7.1).
2. The designer opens the link and signs in with GitHub if asked. The app runs **in the
   designer's own browser** (§3.6): real scrolling, typing, text selection and devtools. Requests
   travel end-to-end encrypted to the host, which forwards them to the dev server. When the
   browser cannot run the tunnel, the screen uses the host's screencast instead (§3.7).
3. **Annotate** turns clicks into picks (click = element, drag = area). Each pick gets a note.
   **Send** delivers the notes (§3.3).
4. Hot reload and reloads show the agent's changes at the same link. Sent notes stay listed with
   their state (held, queued, delivered).

### 3.2 Review devices

A review device is a share device with `review: {preview, sid}`. `Limit` gains `preview`. The
preview's pane and workspace (its home, `preview_home`) are stored too, for display and
notifications. Its scope is `view`.

A review device does **not** get the normal view methods. It has its own complete allow-list,
checked by one function, `review::authorize(device, method, params)`, before the scope and
host-wide checks, which never see a review device's call:

| Method | Rule |
|---|---|
| `hello`, `ping`, `relay.ticket` | always |
| `review.info` | → `{preview {id, label, url, status}, owner, notes, expires_at}` for its preview |
| `tunnel.open {preview}` | its preview only; `host`, `port`, `machine` are refused |
| `tunnel.send`, `tunnel.ack`, `tunnel.close` | streams it opened |
| `browser.preview_open` | its preview; the gateway adds `confine: true` (§3.7) |
| `browser.screencast_frame`, `browser.input`, `browser.navigate` (`back`/`forward`/`reload` only), `browser.session_close` | sessions it opened |
| `browser.annotation_add`, `browser.annotation_pick` | target forced to `{preview}`; `author` injected; picks only in its own session |
| `browser.annotations`, `browser.annotation_update`, `browser.annotation_remove`, `browser.annotations_clear`, `browser.annotations_send` | its own drafts only; `agent`, `draft` of others and `copy_only` refused |

A review device does not use the normal host connection bootstrap (`dashboard.get`,
`events.subscribe`). `hello` reports `kind: "share"` with `review: {preview}`, and the client
opens a **review connection**: `hello`, then `review.info`, then `browser.annotations` polling
every 3 s for note states. It never calls the dashboard or subscribes to events.

Every other method is refused, including `pane.*`, `fs.*`, `git.*`, `agent.*`, `tui.*` (the
WASM terminal transport refuses review devices too) and `browser.attach_screencast`. The
gateway rewrites the params it forwards: it sets the target and author itself and drops fields
the device may not choose.

### 3.3 Trust: direct or held notes

A review share has `notes: "direct" | "held"`:

- `direct`: **Send** delivers the notes to the agent right away, like any annotation (spec 06
  §B11): to a waiting `browser.feedback`, as a follow-up prompt, typed into an idle terminal
  agent, or queued.
- `held`: **Send** puts the draft in state `held`. The owner gets a notification ("3 review notes
  from @alice on v4") and sees the draft in the preview's notes list in the TUI, the desktop and
  the phone, with **Send to agent** and **Discard**. Only an owner device (Full, no limit) or a
  local client can release a held draft (`browser.annotations_release {draft}`,
  `browser.annotations_discard {draft}`). The reviewer sees "waiting for <owner>".

Default: `direct` with `access: github`, `held` with `access: link`.

### 3.4 Authors

`Draft` gains `author: Option<Author {kind: "github"|"share", id, label}>`. Drafts are keyed by
`(workspace, target, author)`, so two reviewers on one preview never edit each other's notes.
The prompt names the author: `Browser feedback from @alice (GitHub) on <url>…`. Events and
`browser.annotations` results carry `author`. Owner clients see every draft with its author.

### 3.5 Creating a review share

`share.create {kind: "review", preview, access, github?, uses?, notes?, ttl_s?, label?}`.
Default TTL 24 h, max 7 days. The gateway picks `sid` (16 random base32 characters). The result
adds `preview {id, label, url}` and `sid`.

### 3.6 The preview in the reviewer's browser

**Two origins.** The review screen is a route of the browser app, `<app>/#/review`. It is the
*controller*: it pairs, holds the keys, the Noise channel and the cookie jar, and shows the
toolbar and the notes.
The preview itself runs on a separate *content origin*, `https://<sid>.p.<app domain>`, in a
full-window cross-origin iframe. The client derives `sid` from what the pairing authenticated:
the first 16 base32 characters of sha256(`"vibeke-review-origin/1\0" ‖ hk ‖ "\0" ‖ host id ‖
"\0" ‖ pid`). Neither the link nor the host chooses it, so a hostile host can never share another
review's origin. Every review link has its own content origin, so cookies and storage never mix
between links. The preview's code cannot reach the controller's storage or
keys, because the origins differ.

**Content origin bundle.** `web/apps/review-content` is a small static bundle served for
`*.p.<app domain>` (wildcard domain and certificate): a bridge page (`/__vk/bridge`), the
service worker (`/__vk/sw.js`) and the injected client script (`/__vk/inject.js`). It holds no
secrets. The service worker never intercepts `/__vk/*`; those come from the static host.

**Boot.**

1. The controller loads a hidden iframe `https://<sid>.p.<domain>/__vk/bridge`. The bridge page
   registers the service worker with scope `/` and posts `ready` to its parent.
2. The controller checks the message origin (exactly its content origin) and transfers a
   `MessagePort` to the bridge. The bridge stays loaded for the whole review.
3. The controller loads the visible iframe at `https://<sid>.p.<domain><preview path>`. The
   service worker serves every request of that frame, including navigations and reloads.

**Worker restarts.** Browsers stop idle service workers. A restarted worker has no port. On its
first fetch it finds the bridge client (`clients.matchAll({includeUncontrolled: true})`, URL
`/__vk/bridge`) and asks for a channel. The bridge creates a `MessageChannel`, keeps one end and
relays it to the controller port, and gives the other end to the worker. The fetch waits for the
channel (at most 10 s, then a 502 page that says the review lost its connection).

**Requests.** For each fetch the service worker sends `{id, method, url, headers, body}` over the
port. The controller opens a tunnel stream (`tunnel.open {preview}`), writes the HTTP/1.1 request
with `Host`, `Origin` and `Referer` rewritten to the preview's `localhost:<port>` and the
`Cookie` header from its jar, parses the response, and streams status, headers and body back
without `Set-Cookie`. The service worker builds the `Response`.
`Location` headers that point at the preview's localhost origin are rewritten to the content
origin. The gateway resolves `{preview}` to the preview's machine and port on the server, then
uses `preview.connect` as the desktop app does (spec 06 §B3.5).

The controller accepts only these messages, validated field by field: `fetch`, `ws.*`,
`pick`, `cookies.*`. A malicious page can only make requests to the one preview it is already
allowed to reach.

**Cookies.** A service worker cannot install cookies from a response it builds, and nothing
stored on the content origin is hidden from the preview's own script. So the cookie jar lives in
the controller, on the app origin, in memory and in the controller's IndexedDB keyed by `sid`.
The controller stores `Set-Cookie` headers from responses (all attributes, including
`HttpOnly`, `Expires`, `Path`), adds the matching `Cookie` header to requests, and strips
`Set-Cookie` before the response reaches the content origin. The injected script replaces
`document.cookie` with a getter and setter over the non-HttpOnly cookies only, which the
controller sends with each HTML response and on change (`cookies.*` messages). Cookie-based
login therefore works, and HttpOnly cookies never reach the content origin. `SameSite` is
ignored: everything is same-origin from the dev server's view.

**WebSockets.** Service workers cannot catch WebSockets. `inject.js`, added to the start of HTML
responses by the service worker, replaces `window.WebSocket` with a shim: it sends `ws.open`
over a `MessageChannel` to the controller, which opens a tunnel stream with an HTTP upgrade. Hot
reload works, so the designer sees changes as the agent saves them.

**Annotations.** `inject.js` also contains the annotate overlay (`annotate-js.ts`). The
controller toggles it with a message. A pick sends `{kind, selector, role, name, text, html,
rect, covers, url, viewport}` to the controller, which calls `browser.annotation_add` (no image).
The server renders a matching crop in its headless browser for the same URL and viewport when it
can (`capture: true`), in a confined session (§3.7). A note without a crop still carries
selector, text and rect.

**Limits.** Absolute URLs to other ports (`http://localhost:3000` for an API) fail; a later
phase can map more declared ports to `<sid>-<port>` origins. A browser without service workers
uses the screencast. At most 32 concurrent tunnel streams per review device. Large downloads go
through the tunnel's flow control and are slower than on a LAN.

### 3.7 Screencast fallback and confined sessions

The controller shows the screencast (spec 06 §B3.6) when the content origin cannot start, or
when the reviewer picks **Use host browser**. The session is confined:

- `browser.session_open {preview, confine: true}` (server) sets a network policy for the
  session's filtering proxy (`agent_browser`): requests may reach this preview's port only, plus
  public internet hosts as for any browser; other previews and other loopback ports are
  refused.
- `browser.navigate` accepts only `back`, `forward` and `reload`.
- The server advertises `browser_confine` in its features. The gateway refuses
  `browser.preview_open` for review devices when the server lacks it, so an older server never
  ignores `confine` silently.

The crop renders for annotations (§3.6) use the same confined policy.

## 4. Continue this session

### 4.1 Flow

1. The sender runs `vibeke handoff link --pane %3` (or presses `l` in the Handoffs view). The
   gateway starts a job: export the session as a handoff bundle (spec 16 §15.2: same format,
   redaction and exclusions), encrypt it, upload it to the account server. The CLI waits for the
   job and prints the link and a Markdown snippet for a pull request.
2. A teammate opens the link (app) or pastes it (TUI, Connections → Hosts `p`). The screen shows
   the brief carried in the link and asks which of the teammate's hosts should take it.
3. That host downloads the blob with its own account, decrypts it, checks it, and adds it as an
   incoming handoff from a teammate. The existing accept flow follows: choose clone and
   worktree, then resume the agent.

The sender's host can be offline from step 2 on.

### 4.2 The link

```
<app>/#/continue?d=<b64url json ContinueLink>
ContinueLink = {v:1, server, blob, k, sha256, size, exp, brief}
brief = {repo, branch, harness, title, last, from}
```

- `server`: account server URL. `blob`: blob id. `k`: 32-byte key, base64url. `sha256` and
  `size` of the plaintext bundle.
- `brief`: what the screen shows before anything is downloaded: repo name, branch, harness, task
  title, the agent's last message (at most 280 characters), and the sender's GitHub login. The
  brief is as private as the link: only link holders see it, and the account server never does.
- A link longer than 4 KiB is refused at creation (the brief is truncated first).

### 4.3 Encryption

The bundle is encrypted with XChaCha20-Poly1305 in the STREAM construction (`StreamBE32` of the
RustCrypto `aead` crate): a 19-byte random nonce prefix, a 4-byte big-endian chunk counter and a
1-byte last-chunk flag make the 24-byte nonce.

```
file = "VKC1" ‖ prefix[19] ‖ chunk* ‖ last_chunk
chunk = encrypt_next(1 MiB plaintext)        (1 MiB + 16 bytes)
last_chunk = encrypt_last(0..1 MiB plaintext) (≤ 1 MiB + 16 bytes)
AAD = "vibeke-continue/1"
```

Decryption fails on a bad tag, on bytes after the last chunk, and when the file ends without a
last chunk. The receiver then checks `sha256` and `size` of the plaintext before handing the file
to the server. Test vectors live in `vk-handoff`.

### 4.4 Who may fetch

The blob carries `access` like a share (§2.2). The download uses the recipient host's account
token (`vibeke login`). With `access: github` the account server checks the account's GitHub id
against the blob's list, or that the account is a current member of one of the blob's teams
(§2.5). With `access: link` any signed-in account may fetch it; the key in the link is the
secret. Without an account server, continue links are unavailable.

Blobs live 7 days by default (`ttl_s`, max 30 days). The sender can delete one
(`handoff.link.revoke`). Quota per account: 2 GiB in total, 256 MiB per blob.

### 4.5 Gateway methods

| Method | Params → result |
|---|---|
| `handoff.link.create` | `{pane, access, github?, teams?, ttl_s?, interrupt?}` → `{job}` |
| `handoff.link.status` | `{job}` → `{state: exporting\|uploading\|done\|failed, progress?, error?, link?, markdown?, blob?, expires_at?}` |
| `handoff.link.list` | `{}` → `{links:[{blob, job, pane, brief, created_at, expires_at, size}]}` |
| `handoff.link.revoke` | `{blob}` → `{}` |
| `handoff.link.redeem` | `{link}` → `{job}`; `handoff.link.status` then returns `incoming` |

All are owner methods: own Full devices without a limit, and the server bridge (`gateway.call`).
Panes may not call them. Jobs are stored like `handoff.send` jobs, survive a gateway restart (an
interrupted job is marked `failed: interrupted`), and can be cancelled with `handoff.cancel`.

`handoff.link.redeem` downloads, decrypts and verifies, then delivers to the server with the same
code path as a peer handoff (`deliver_to_server`), with `from: {host: "link:<blob>",
owner: "teammate", name: <brief.from>, account: {id, login}}`. The sender identity comes from the
account server's blob metadata (the uploading account, authenticated), not from the link. A
continue link is never imported automatically, even when the sender is the same person.

### 4.6 Pull request

`vibeke handoff link --pane %3 --pr` adds the snippet as a comment to the branch's pull request
with the user's `gh` (`gh pr comment`). Without `--pr`, nothing is posted; the snippet is
printed. The snippet:

```markdown
**Continue this session**: Claude Code on `feat/login`, sent by @alice, link valid until 21 Oct.
[Open in Vibeke](https://app.vibeke.dev/#/continue?d=…)
```

## 5. Unattended sharing

### 5.1 Who can call

A program outside every pane on the host (same user, the 0600 socket) has full access to the
vibeke/1 API (spec 09 §3.2 as built). A Hermes messaging gateway on the host is such a caller and
needs no new credential. Panes stay refused for share and continue-link creation
(`PANE_FORBIDDEN_CALLS`).

### 5.2 CLI verbs

New verbs in `vk-cli/src/verbs.rs`, over `gateway.call`:

| Command | Calls |
|---|---|
| `vibeke share watch [--pane P \| --workspace W] [--scope view\|approve] [--github a,b] [--team T] [--any-github \| --link] [--uses N] [--ttl 24h] [--label L] [--json]` | `share.create {kind:"share"}` |
| `vibeke share review <preview> [--github …] [--team T] [--any-github \| --link] [--notes direct\|held] [--uses N] [--ttl …] [--label L] [--json]` | `share.create {kind:"review"}` |
| `vibeke share list [--json]` | `share.list` |
| `vibeke share revoke <id>` | `share.revoke` |
| `vibeke handoff link [--pane P] [--github …] [--team T] [--any-github \| --link] [--ttl 7d] [--pr] [--json]` | `handoff.link.create`, then `handoff.link.status` until done |
| `vibeke handoff open <link> [--json]` | `handoff.link.redeem`, then status |

Without `--github`, `--team`, `--any-github` or `--link`, the verbs send `access: github` with the
`[sharing] github` list. `--team` (repeatable or a comma list, a name or an id) sends `teams`
and combines with `--github`; without `--github` it sends no `github` list. Without `--json` the command prints the link alone, so a script can post
it. Errors name the fix: `vibeke gateway on`, `vibeke login`, or "set `[sharing] github` or pass
`--github`".

### 5.3 The Slack flow

```
Slack: "@hermes add a dark mode toggle to the settings page"
Hermes (on the host, outside any pane):
  vibeke workspace create --cwd ~/code/app --name dark-mode --json   → workspace, root pane
  vibeke agent start --pane <root> --harness claude --json           → run
  vibeke agent prompt <run> "Add a dark mode toggle … Declare the preview when the dev server runs."
  vibeke share watch --workspace <ws>                                 → watch link
  (post the watch link in the thread)
  vibeke events wait --types preview.declared                         → preview
  vibeke share review <preview>                                       → review link
  (post the review link in the thread)
  vibeke agent wait <run>
  after the PR: vibeke handoff link --pane <root> --pr                → continue link
```

The links are GitHub-gated and multi-use, so every teammate in the channel signs in once and
gets their own device. Unfurl bots never see the fragment.

### 5.4 Hermes skill for the messaging gateway

The existing Vibeke skill stops outside a pane (`test "$VIBEKE" = 1`). A second skill,
`vibeke-remote`, is installed with the Hermes integration at
`<HERMES_HOME>/skills/vibeke-remote/SKILL.md`. It teaches the §5.3 flow and these rules: post
links, never terminal output; ask the requester before using `--any-github` or `--link`; tell
the requester which GitHub accounts can open a link.

## 6. Account server (vibeke-cloud) changes

### 6.1 Attestation keys

A new Ed25519 key (`--attest-key-file`, generated on first start if missing, mode 0600).
`GET /v1/keys` → `{attest: [{kid, alg: "ed25519", pub}]}`. `kid` is the first 8 bytes of
sha256(pub), hex. A second `--attest-key-file` may be given during rotation: the first signs, all
are published.

### 6.2 GitHub users and web sign-in

| Route | Auth | Does |
|---|---|---|
| `GET /v1/github/users/{login}` | Bearer account | `{id, login}` from the GitHub API (cached 1 h); 404 `unknown_user` |
| `GET /v1/attest/start?aud&nonce&return` | none | checks `return` against the app origins and the parameter shapes; stores them in a signed cookie (`vk_attest`, `Path=/v1/attest`, 10 min, HttpOnly, Secure, SameSite=Lax); redirects to GitHub (`read:user`) |
| `GET /v1/attest/callback` | cookie | exchanges the code, upserts the account, signs the attestation, clears the cookie, redirects to `<return>#/attest?t=<attestation>` |

The GitHub OAuth app gets a second callback URL (`/v1/attest/callback`). A failed sign-in
redirects to `<return>#/attest?error=<code>`. Rate limits as for the device flow (10/min per IP).

### 6.3 Encrypted blobs

| Route | Auth | Does |
|---|---|---|
| `POST /v1/blobs` | Bearer | `{size, ttl_s?, access, github?: [id], teams?: [team id]}` → `{id, expires_at}`; checks quota; 403 `not_a_member` for a team the creator is not in |
| `PUT /v1/blobs/{id}` | Bearer, owner | streams the ciphertext; refuses more than the declared size; once only |
| `GET /v1/blobs/{id}/meta` | Bearer | `{size, created_at, expires_at, from: {id, login}}` if allowed |
| `GET /v1/blobs/{id}` | Bearer | streams the ciphertext if allowed and uploaded |
| `DELETE /v1/blobs/{id}` | Bearer, owner | deletes |

Storage is a directory (`--blob-dir`) behind a `BlobStore` trait, so an object store can replace
it later. Uploads go to a temporary file and are renamed when complete. A sweeper deletes expired
blobs and unfinished uploads older than 1 h, hourly. Table `blobs (id, account_id, size,
access, github jsonb, created_at, expires_at, uploaded_at)`. The blob routes have their own body
limit (256 MiB + 4 KiB); the 16 KiB limit stays for the rest.

`access: github` allows accounts whose `github_id` is in the list, and current members of the
listed teams (at least one of the two lists is not empty); `["*"]` stored as `null` with
`access: any` allows any account. The owner may always fetch.

### 6.4 Sharing list and account page

The account page, `GET /account`, is a server-rendered page in the Vibeke look. The user signs in
with GitHub (a browser session cookie, `Path=/account`, separate from the API tokens). It shows
**People who can open your links** with an add form (a GitHub username, resolved to its id) and a
**Remove** button for each person, and the account's hosts. Forms carry a CSRF token and are
checked for a same-origin `Origin`.

| Route | Auth | Does |
|---|---|---|
| `GET /v1/sharing` | Bearer | `{people: [{id, login}], teams: [{id, name}]}`; `teams` are the caller's teams ticked "share with by default" (ticked when the user joins) |
| `PUT /v1/sharing` | Bearer | `{people: [login]}` replaces the list (logins resolved to ids) |
| `POST /v1/sharing/people` | Bearer | `{login}` → `{person: {id, login}}` |
| `DELETE /v1/sharing/people/{id}` | Bearer | removes one person |

At most 200 people per account. The gateway reads the list when it makes a GitHub link without a
`github` list and without `teams` (`vk_account::Account::sharing`). An account server without
the route answers 404, which counts as an empty list; one that sends only `people` counts as no
teams.

The page also shows **Teams**: create a team, see its members and pending invitations, invite by
GitHub login or make an invite link (owners), remove members (owners), leave, and tick
or untick "share with by default". Team routes (Bearer):

| Route | Does |
|---|---|
| `GET /v1/teams` | `{teams: [{id, name, role, members}]}`: the caller's teams; `members` is a count |
| `POST /v1/teams` | `{name}` → `{team: {id, name, role: "owner", members: 1}}` |
| `GET /v1/teams/{id}` | members only: `{team, members: [{id, login, role, joined_at}], invites: [{id, login?, link, expires_at}]}` |
| `POST /v1/teams/{id}/invites` | owners: `{login}` → `{invite: {id, login, expires_at}}`, or `{link: true}` → `{invite: {id, url, expires_at}}` (the URL is shown once) |
| `DELETE /v1/teams/{id}/members/{github_id}` | owners, or the member themself (leave) |

## 7. Clients

### 7.1 TUI

- **People** form (`prefix+alt+v`): a new first field **Kind**: *Watch* (today's share) or
  *Design review* (lists the workspace's previews). A new **Who** field: *Anyone with the link*
  (default for Watch, as today), *GitHub: <default list>*, *Team: <name>* for each of the
  account's teams (`gateway.call account.teams`, asked when the form opens; none on older
  gateways), *Any GitHub account*. A team choice sends the team's id. Design review
  adds **Notes**: *Straight to the agent* or *Hold for me*. The created invitation shows uses
  left.
- **People** lists the GitHub login of each device and groups devices under their invitation.
- Held review notes raise a notification and show in the preview's notes split with **Send** and
  **Discard**.
- **Hosts** paste (`p`): a continue link shows its brief and asks to import it on this host
  (`handoff.link.redeem`). Share and review links open in the system browser instead of being
  refused.
- **Handoffs** view: `l` creates a continue link for the focused pane and copies it.

### 7.2 Browser app (PWA and WASM view)

- `#/pair` with `access: github` first sends the user to the account server (§2.3), then pairs.
  A refused login shows "This link is for: @alice, @bob and everyone in Acme" (from the
  refusal's `allowed` and `teams`) and lets them sign in again.
- `#/attest` receives `t` or `error` and resumes the pending pairing from session storage.
- `#/review` pairs like `#/pair`, then shows the review screen: the preview in the content-origin
  iframe (§3.6) or the screencast (§3.7), a slim toolbar (Annotate, Reload, Notes, Use host
  browser), the notes sheet with **Send**, and sent notes with their state. It skips the host list
  and the dashboard. Review connections are stored apart from host records, keyed by
  `(host_id, pid)`, so a review never replaces an owner or watch connection, and two reviews on
  one host stay separate. Reopening the link goes straight to the screen.
- Owner devices: the preview's notes list shows held drafts with **Send to agent** and
  **Discard**.
- `#/continue` parses the link, shows the brief, and lists the user's own hosts with Full access.
  **Continue on <host>** calls `handoff.link.redeem` on that host, waits for the job, and opens
  the incoming handoff.

### 7.3 Desktop app

Deep links `vibeke://continue?d=` and `https://…/#/continue?d=` map to `#/continue`. Review links
and GitHub-gated share links open in the system browser (§2.3), so the review screen has one
implementation.

## 8. Security notes

- A review device reaches one preview: HTTP and WebSocket streams to that preview's port, and
  optionally one confined host browser tab. It cannot reach terminals, files, git, agents, other
  previews or agent browsers (§3.2).
- The preview's code runs on its own origin per link. It cannot read the controller's keys
  (§3.6). The controller accepts only a fixed set of validated messages from it.
- `access: link` stays the default only for person-made watch shares, as today. CLI verbs default
  to GitHub and fail closed (§2.2).
- GitHub accounts are matched by id, never by login. Teams are matched by id, never by name, and
  only through the account server's signature (attestation) or its own check (blobs).
- The account server learns which GitHub account opened a link to which host id, and blob sizes.
  It never learns pairing secrets, blob keys, briefs or bundle contents.
- Continue bundles use today's redaction and exclusions (secret-looking files, untracked files
  over 5 MiB, transcript redaction). The continue screen says what is excluded.
- The attestation is bound to one pid and one device key, and lives 5 minutes.

## 9. Tests

- vk-gateway: review authorization table (allowed, refused, wrong preview, other session, param
  rewriting, `tunnel.open` with host/port refused, `tui.*` refused); multi-use pairing (count,
  expiry, idempotent retry, concurrent claims, reconcile after a crash between writes,
  invitation-wide revoke); attestation checks (bad signature, unknown kid, wrong aud, wrong
  nonce, expired, future iat, id not listed, `*`, `upgrade_required` without attestation, team
  member allowed, non-member and other-team refused);
  continue link jobs with a fake account server (create, status, redeem, sha mismatch,
  truncated file, revoke).
- vk-server: confined session policy refuses other previews and loopback ports; URL navigation
  refused; drafts keyed by author; prompt names the author; a held draft is not delivered until
  released, and only by an owner.
- vk-handoff: STREAM encryption vectors, truncation and trailing-data refusal.
- vk-account: attestation verification vectors shared with vibeke-cloud.
- vibeke-cloud: GitHub user lookup, attest start/callback with a fake GitHub, keys endpoint,
  blob routes (quota, size, once-only upload, access by id, expiry sweep).
- Integration (`crates/vibeke/tests/it`): `share review`, `share watch` and `handoff link` CLI
  paths against a session with a gateway and a fake account server.
- Web: link parsing, attestation resume, review screen message validation, service-worker
  cookie jar, HTTP request/response framing.

## 10. Phases

Phase 1 (this spec): everything above.

Later: GitHub organisation or repository membership as `access` (needs `read:org` or a GitHub
App), an object-store backend for blobs, more ports per review origin, cross-harness
continuation, replies from the agent to a reviewer's note, short links.
