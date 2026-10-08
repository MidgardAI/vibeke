// App API types (spec 16 §7.4) and the normalized entity shapes the gateway returns (§7.5).
// Field names follow crates/vk-proto/src/model.rs. Enums are snake_case: the gateway rewrites
// interaction enums; `normalize*` below applies the same rewrite defensively to anything
// still in serde's PascalCase (e.g. AgentRun facets), so the UI only ever sees one form.

export type Risk = 'low' | 'medium' | 'high' | 'unknown';
export type InteractionKind = 'approval' | 'question' | 'plan_review' | 'notice';
export type InteractionStatus = 'open' | 'answered' | 'resolved_elsewhere' | 'expired' | 'cancelled';
export type DeliveryState =
  | 'none'
  | 'decision_recorded'
  | 'delivering'
  | 'delivered'
  | 'delivery_unknown'
  | 'failed'
  | 'superseded'
  | 'resolved_elsewhere';
export type AnswerChannel = 'native' | 'keystrokes' | 'none';
export type Decision = 'allow' | 'allow_always' | 'deny';
export type StateSource = 'structured' | 'self_report' | 'screen' | 'process' | 'user';
export type Execution = 'starting' | 'working' | 'idle' | 'error' | 'rate_limited' | 'exited' | 'unknown';
export type AdapterHealth = 'healthy' | 'degraded' | 'disconnected' | 'unvalidated_version';
export type Scope = 'full' | 'approve' | 'view';

export interface ActionInfo {
  tool: string;
  summary: string;
  command: string | null;
  paths: string[];
  diff: string | null;
  risk: Risk;
  risk_reasons: string[];
}

export interface QuestionOption {
  id: string;
  label: string;
  description: string | null;
}

export interface Question {
  id: string;
  prompt: string;
  header: string | null;
  multi: boolean;
  options: QuestionOption[];
  allow_free_text: boolean;
}

export interface Answer {
  decision: Decision | null;
  /** [question id, chosen option ids] pairs (serde tuple form). */
  choices: [string, string[]][];
  text: string | null;
}

export interface Interaction {
  id: string;
  handle: string;
  run: string;
  pane: string;
  kind: InteractionKind;
  status: InteractionStatus;
  title: string;
  body_md: string | null;
  action: ActionInfo | null;
  questions: Question[];
  plan_md: string | null;
  answer_channel: AnswerChannel;
  native_ref: string | null;
  source: StateSource;
  confidence: number;
  answerable: boolean;
  gate: boolean;
  decision_rev: number;
  delivery: DeliveryState;
  delivery_error: string | null;
  answer: Answer | null;
  answered_by: string | null;
  opened_at_ms: number;
  answered_at_ms: number | null;
  /** Added by the gateway (batch grouping, §7.6). */
  harness?: string;
  /** Added by the gateway: repo root of the run's cwd. */
  repo_root?: string;
}

export interface Facet<T> {
  value: T;
  since_ms: number;
  source: StateSource;
  confidence: number;
  detail: string | null;
}

export interface AgentRun {
  id: string;
  handle: string;
  name: string | null;
  pane: string;
  harness: string;
  harness_version: string | null;
  integration: string;
  harness_session_id: string | null;
  transcript_path: string | null;
  resume_argv: string[];
  cwd: string | null;
  model: string | null;
  task: string | null;
  execution: Facet<Execution>;
  health: AdapterHealth;
  yolo: boolean;
  permission_mode: string | null;
  last_message: string | null;
  last_tool: string | null;
  turns_completed: number;
  /** Bumped when execution goes idle after work (done vs idle). */
  done_rev: number;
  started_at_ms: number;
  ended_at_ms: number | null;
  capabilities: string[];
}

export interface Workspace {
  id: string;
  handle: string;
  name: string | null;
  auto_name: string;
  root_path: string;
  task: string | null;
  order: number;
  branch: string | null;
}

/** serde externally tagged enum, passed through untouched. */
export type LayoutNode =
  | { Leaf: { pane: string } }
  | { Split: { dir: 'Horizontal' | 'Vertical'; children: [LayoutNode, number][] } };

export interface Tab {
  id: string;
  handle: string;
  workspace: string;
  title: string | null;
  number: number;
  layout: LayoutNode;
  focused_pane: string | null;
  zoomed_pane: string | null;
  order: number;
}

export interface Pane {
  id: string;
  handle: string;
  tab: string;
  workspace: string;
  title: string | null;
  auto_title: string;
  cwd: string | null;
  cols: number;
  rows: number;
  child_pid: number | null;
  fg_cmdline: string[];
  exited: boolean;
  exit_code: number | null;
  unread: boolean;
  marked_unread: boolean;
  pinned: boolean;
  created_by: string;
  recovered: string | null;
}

export interface Task {
  id: string;
  handle: string;
  title: string;
  slug: string;
  workspace: string | null;
  repo_root: string;
  worktree_path: string | null;
  branch: string | null;
  status: string;
  created_at_ms: number;
  [k: string]: unknown;
}

export interface Notification {
  id: string;
  kind: string;
  pane: string | null;
  title: string;
  body: string;
  urgency: string;
  created_at_ms: number;
  read: boolean;
}

export const displayName = (w: Workspace): string => w.name ?? w.auto_name;
export const paneTitle = (p: Pane): string => p.title ?? p.auto_title;

/** Gateway event (§7.5): `event` notification params. */
export interface AppEvent {
  seq: number;
  ts: number;
  type: string;
  subject: Record<string, string | undefined>;
  data: Record<string, unknown>;
}

/** A dev-server preview the host detected or an agent declared (vk-proto `Preview`). */
export interface Preview {
  id: string;
  handle: string;
  machine?: string;
  pane: string | null;
  task?: string | null;
  port: number;
  path: string;
  label: string | null;
  url: string;
  scheme?: string;
  /** `suggested` | `declared` | `up` | `down` | `gone`. */
  status: string;
  source?: string;
  first_seen_ms?: number;
  last_seen_ms?: number;
}

export interface Dashboard {
  /** Snapshot barrier: subscribe with `after = at`. */
  at: number;
  session: string;
  machine: string;
  workspaces: Workspace[];
  tabs: Tab[];
  panes: Pane[];
  runs: AgentRun[];
  interactions: Interaction[];
  tasks: Task[];
  /** Full-scope devices only (shares never see previews in the snapshot). */
  previews?: Preview[];
  notifications_unread: number;
}

export interface GitFile {
  path: string;
  orig_path?: string | null;
  x: string;
  y: string;
  /** `modified` | `added` | `deleted` | `renamed` | `untracked` | `conflicted`. */
  kind: string;
  staged?: boolean;
  adds?: number | null;
  dels?: number | null;
  binary: boolean;
  secret?: boolean;
}

export interface GitStatus {
  repo_root: string;
  branch?: string | null;
  upstream?: string | null;
  ahead: number;
  behind: number;
  files: GitFile[];
  clean: boolean;
  truncated?: boolean;
}

export interface GitDiff {
  file?: string;
  diff: string;
  truncated: boolean;
  binary: boolean;
  untracked: boolean;
  secret?: boolean;
}

/** One file of a `git.diff {base|range}` listing (no `file`). */
export interface GitRevFile {
  path: string;
  adds?: number | null;
  dels?: number | null;
  binary: boolean;
  secret?: boolean;
  /** git's name-status letter (A added, M modified, D deleted, R renamed, C copied, T type change). */
  status?: 'A' | 'M' | 'D' | 'R' | 'C' | 'T' | null;
  /** Source path of a rename / copy. */
  orig_path?: string | null;
}

/** `git.diff {pane, base | range}` without `file`: the files that differ. */
export interface GitRevFiles {
  rev: string;
  files: GitRevFile[];
  truncated: boolean;
}

/** `git.diff` params: the working tree (`file`, `staged`), or against `base` / over `range`. */
export type GitDiffParams =
  | { pane: string; file: string; staged?: boolean }
  | { pane: string; base: string; file?: string }
  | { pane: string; range: string; file?: string };

/** `git.log` entry; `ts` is unix ms. */
export interface Commit {
  sha: string;
  short: string;
  author: string;
  ts: number;
  subject: string;
}

export interface GitLog {
  commits: Commit[];
  truncated: boolean;
}

export type FsEntryKind = 'file' | 'dir' | 'symlink' | 'other';

/** One `fs.list` entry (one directory level; secrets are listed but never readable). */
export interface FsEntry {
  name: string;
  kind: FsEntryKind;
  size?: number | null;
  ignored: boolean;
  secret: boolean;
}

export interface FsList {
  path: string;
  entries: FsEntry[];
  truncated: boolean;
  /** The directory itself looks like a secret: no entries. */
  secret?: boolean;
}

/** One `fs.browse` entry: a directory (a `.git` inside marks a repository). */
export interface BrowseEntry {
  name: string;
  git_repo: boolean;
}

/** `fs.browse`: the host's directories under $HOME and configured roots (full scope). */
export interface BrowseResult {
  /** The canonical directory listed. */
  path: string;
  /** Its parent, or null when that is outside the browsable roots. */
  parent: string | null;
  git_repo: boolean;
  entries: BrowseEntry[];
  truncated: boolean;
}

/** One clone whose remote matches the asked origin (`repo.candidates`). */
export interface RepoCandidate {
  path: string;
  remote: string;
}

export interface FsRead {
  path: string;
  text?: string | null;
  binary: boolean;
  truncated: boolean;
  size: number | null;
  secret: boolean;
}

export interface Worktree {
  path: string;
  branch: string | null;
  head: string | null;
  locked: boolean;
  prunable: boolean;
  main: boolean;
}

export type TranscriptItemKind = 'text' | 'thinking' | 'tool_call' | 'tool_result';

/** One item of a transcript turn (server gateway_api.rs `line_items`). */
export interface TranscriptItem {
  kind: TranscriptItemKind | string;
  /** `text` items: `user` | `assistant` (others possible). */
  role?: string | null;
  text?: string | null;
  /** `tool_call`: the tool name. */
  tool?: string | null;
  /** `tool_call` input / `tool_result` output, one line, ≤ 160 chars. */
  summary?: string | null;
  /** Tool call id (pairs a call with its result). */
  id?: string | null;
  /** `tool_result`: the tool failed. */
  error?: boolean | null;
  /** Epoch ms of the transcript line (null when the line has none; older servers omit it). */
  ts?: number | null;
}

/** One transcript turn (`agent.transcript`): a user prompt and everything up to the next one. */
export interface TranscriptTurn {
  /** Stable index from the start of the transcript (1-based). */
  n: number;
  ts?: string | number | null;
  items: TranscriptItem[];
  /** Last item ts − turn start; null when unknown (older servers omit it). */
  duration_ms?: number | null;
  /** Tool calls in the turn. */
  tool_count?: number | null;
  /** Tool calls that start a subagent (Task/Agent…). */
  subagent_count?: number | null;
}

export interface TranscriptPage {
  run: string;
  turns: TranscriptTurn[];
  /** Pass as `before` to load older turns; null/absent = this page reaches the start. */
  next_before?: number | null;
}

/** A styled run within a row (`pane.read` source `styled`); plain spans are omitted. */
export interface StyledRun {
  /** Start column (terminal cells, not characters: a wide character covers two). */
  start: number;
  /** Width in cells. */
  len: number;
  /** null = default, 0–255 = palette index, `#rrggbb` = truecolor. */
  fg: number | string | null;
  bg: number | string | null;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
  inverse: boolean;
}

export interface StyledRow {
  /** One grapheme per cell; the spacer cell of a wide character is omitted. */
  text: string;
  wrapped: boolean;
  runs: StyledRun[];
}

export interface StyledScreen {
  pane: string;
  cols: number;
  rows: StyledRow[];
  cursor: { col: number; row: number; visible: boolean } | null;
}

export interface PaneText {
  text: string;
  revision: number;
}

export interface HarnessInfo {
  id: string;
  display: string;
  capabilities: string[];
  version_detected: string | null;
}

export interface DeviceInfo {
  id: string;
  name: string;
  platform: string;
  scope: Scope;
  paired_at: number;
  fingerprint: string;
  push: boolean;
  this: boolean;
  /** `device` | `share` | `handoff` | `peer` (spec 16 §15). */
  kind?: string;
  /** Unix seconds; shares and handoff invitations expire. */
  expires_at?: number | null;
  limit?: { workspace?: string; pane?: string } | null;
  /** For `peer` devices: whose host it is and how it introduced itself. */
  peer?: { owner: PeerOwner; host_name?: string; user?: GitUser } | null;
}

/** `self`: one of the owner's own hosts; `teammate`: a host that redeemed a handoff invitation. */
export type PeerOwner = 'self' | 'teammate';

export interface GitUser {
  name?: string | null;
  email?: string | null;
}

/** A host this one can hand work to (`peer.list`; private keys never leave the host). */
export interface PeerInfo {
  id: string;
  name: string;
  relay: string;
  host: string;
  device_id: string;
  owner: PeerOwner;
  added_at: number;
  expires_at: number | null;
  expired: boolean;
}

/** Where an outgoing handoff stands (`handoff.send`, spec 16 §15.2). */
export type HandoffJobState = 'queued' | 'exporting' | 'sending' | 'delivered' | 'failed' | 'cancelled';

/** An outgoing handoff job, kept by the server and run by this host's gateway (`handoff.job` events carry it). */
export interface HandoffJob {
  id: string;
  pane: string;
  /** Peer id (see `handoff.peers`) and its name when the job was created. */
  peer: string;
  peer_name: string;
  interrupt: boolean;
  state: HandoffJobState;
  /** Bytes the destination has, of `total` (0 until the export is done). */
  sent: number;
  total: number;
  /** The destination's incoming handoff id and its state there (`pending`, `imported`, …). */
  incoming?: string;
  incoming_state?: IncomingHandoffState | null;
  /** Why it failed (a message, or a JSON-RPC style error object). */
  error?: string | { kind?: string; message?: string } | null;
  /** Unix seconds. */
  created_at: number;
  updated_at: number;
}

/** A host this one can hand work to (`handoff.peers`; no keys or addresses). */
export interface HandoffPeer {
  id: string;
  name: string;
  owner: PeerOwner;
  added_at?: number;
  /** Unix seconds; absent or null: never (own hosts). */
  expires_at?: number | null;
  expired?: boolean;
}

/**
 * A pane asks the user to run one specific call it may not make itself (spec 09 §3.2 "Approved
 * calls", `auth.approve`): `handoff.send`, `handoff.cancel` of its own jobs, or
 * `gateway.call {method: "peer.redeem"}`. The user decides outside the pane (`auth.approve.decide`).
 */
export interface ApprovalRequest {
  request: string;
  kind: 'approval';
  /** The asking pane (id, handle) and its workspace. */
  pane: string;
  pane_handle: string;
  workspace: string;
  method: string;
  /** The frozen params (an invitation link shows as `(hidden)`). */
  params: Record<string, unknown>;
  /** What will happen, computed by the host from its own facts (never from the pane's text). */
  summary: string;
  facts: Record<string, unknown>;
  /** The pane's own words: unverified. */
  reason: string;
  reason_verified: false;
  peer: { id: string; name: string; owner: PeerOwner | string } | null;
  /** Whether `always` may be chosen (never for peer.redeem). */
  always_allowed: boolean;
  created_at_ms: number;
  status: 'pending' | 'running' | string;
}

/** A standing grant (`always`): the same call from that pane to that peer runs without asking until the pane restarts. */
export interface ApprovalGrant {
  pane: string;
  method: string;
  target: string | null;
  peer: string;
  peer_name: string;
  request: string;
  created_at_ms: number;
}

export type ApprovalDecision = 'approve' | 'always' | 'deny';

/** A pending invitation on this host (`share.list`). */
export interface InvitationInfo {
  /** Pairing id; `share.revoke {id}` cancels it. */
  id: string;
  kind: 'share' | 'handoff' | 'peer' | 'device';
  scope: Scope;
  label: string | null;
  limit: { workspace?: string; pane?: string } | null;
  /** Unix seconds; null for invitations from older gateways. */
  created: number | null;
  /** Unix seconds the link must be opened by. */
  link_expires_at: number;
  /** Unix seconds the resulting device stops working; null: never (own hosts). */
  device_expires_at: number | null;
}

/** A device an invitation produced (`share.list`); `share.revoke {id}` revokes it. */
export interface InvitedDeviceInfo {
  id: string;
  kind: 'share' | 'peer';
  name: string;
  scope: Scope;
  paired_at: number;
  owner: PeerOwner | null;
  sender: { host_name?: string | null; user?: GitUser | null } | null;
  expires_at: number | null;
  limit: { workspace?: string; pane?: string } | null;
}

/** What an accepted handoff became on the receiving host (vk-server handoff.rs `run_accept`). */
export interface HandoffImportResult {
  repo?: string;
  cloned?: boolean;
  worktree?: string;
  branch?: string;
  cwd?: string;
  resumed?: boolean;
  resume_args?: string[] | null;
  /** Untracked files the import could not write. */
  not_written?: { path: string; reason: string }[];
  skipped?: { path: string; reason: string }[];
  trust?: { tool: string; status: 'trusted' | 'not_needed' | 'not_installed' | 'failed' | string; dir?: string; error?: string }[];
  workspace?: string | null;
  pane?: string | null;
  run?: unknown;
  /** The import worked but starting the agent failed (JSON-RPC error object). */
  agent_error?: { code?: number; message?: string; data?: { kind?: string } };
  /** The import worked but no workspace could be opened on it. */
  workspace_error?: string;
}

export type IncomingHandoffState = 'pending' | 'importing' | 'imported' | 'failed' | 'declined';

/** A handoff waiting on (or imported by) the receiving host (vk-server handoff.rs `view`). */
export interface IncomingHandoff {
  id: string;
  from: { host: string; owner: 'self' | 'teammate'; user?: string };
  manifest: {
    source_host: string;
    repo_name: string;
    origin: string | null;
    branch: string | null;
    head: string;
    harness: string | null;
    session_id: string | null;
    cwd_rel: string;
    skipped: { path: string; reason: string }[];
    last_message: string | null;
    untracked: number;
    transcript: boolean;
    redactions: number;
    created_at: number;
  };
  size: number;
  bundle_path: string | null;
  state: IncomingHandoffState;
  error: { kind: string; message: string; details?: unknown } | null;
  result: HandoffImportResult | null;
  created_at_ms: number;
  updated_at_ms: number;
  expires_at_ms: number;
}

/** `handoff.prefs`: placement the receiving host remembers, and whether own handoffs always wait. */
export interface HandoffPrefs {
  always_ask: boolean;
  placement: Record<string, { repo: string; worktree_parent: string }>;
  repos: string[];
}

/** Per-device push/notification prefs held by the gateway (`prefs.get/set`). */
export interface DevicePrefs {
  privacy: 'full' | 'summary' | 'minimal';
  notify_input: boolean;
  notify_done: boolean;
}

export interface BatchResult {
  interaction: string;
  ok: boolean;
  result?: { interaction: Interaction; delivery: { channel?: string } | DeliveryState };
  error?: { code: number; message: string; data?: { kind?: string } };
}

/** Method → params/result map for the app API (spec 16 §7.4). `op_id` is added by RpcClient. */
export interface AppApi {
  hello: {
    params: { client: string; version: string; visible: boolean };
    result: {
      host_name: string;
      host_id?: string;
      device_id: string;
      scope: Scope;
      server_version: string | null;
      gateway_version?: string;
      features: string[];
      /** `device` | `share` | `handoff` (spec 16 §15). */
      kind?: string;
      /** Unix seconds; null for an ordinary device. */
      expires_at?: number | null;
      limit?: { workspace?: string | null; pane?: string | null } | null;
    };
  };
  'client.visibility': { params: { visible: boolean }; result: Record<string, never> };
  'dashboard.get': { params: Record<string, never>; result: Dashboard };
  'pane.read': {
    /** `styled` returns StyledScreen (servers with the gateway X1 pieces); the others PaneText. */
    params: { pane: string; source?: 'visible' | 'recent' | 'scrollback' | 'styled' | string; lines?: number };
    result: PaneText | StyledScreen;
  };
  'pane.send_text': { params: { pane: string; text: string; submit?: boolean }; result: Record<string, never> };
  'pane.send_keys': { params: { pane: string; keys: string[] }; result: Record<string, never> };
  'pane.rename': { params: { pane: string; title: string | null }; result: { pane: Pane } };
  'pane.close': { params: { pane: string }; result: unknown };
  'pane.focus': { params: { pane: string }; result: unknown };
  'agent.prompt': { params: { target: string; text: string }; result: Record<string, never> };
  'agent.interrupt': { params: { target: string }; result: Record<string, never> };
  'agent.transcript': {
    /** `limit` ≤ 200; `before` = a page's `next_before` (turns with n < before). */
    params: { target: string; limit?: number; before?: number };
    result: TranscriptPage;
  };
  'agent.start': {
    params: { workspace: string; cwd?: string; harness: string; prompt?: string };
    /** `pane` is the new pane id (the gateway returns the server's agent.start result + pane). */
    result: { run?: AgentRun; pane: string; [k: string]: unknown };
  };
  'agent.harnesses': { params: Record<string, never>; result: { harnesses: HarnessInfo[] } };
  'tab.create': { params: { workspace: string; cwd?: string; title?: string }; result: { tab: Tab; root_pane: Pane } };
  'tab.rename': { params: { tab: string; title: string | null }; result: unknown };
  'tab.close': { params: { tab: string }; result: unknown };
  'tab.focus': { params: { tab: string }; result: unknown };
  'preview.open': { params: { preview?: string; url?: string; pane?: string; focus?: boolean }; result: unknown };
  'interaction.list': { params: Record<string, unknown>; result: { interactions: Interaction[] } };
  'interaction.get': { params: { interaction: string }; result: { interaction: Interaction } };
  'interaction.answer': {
    params: {
      interaction: string;
      decision_rev: number;
      decision?: Decision;
      /** Map of question id → chosen option ids (the server's `interaction.answer` form). */
      choices?: Record<string, string[]>;
      text?: string;
    };
    /** `delivery` is `{channel: native|keystrokes|recorded}`; the state is `interaction.delivery`. */
    result: { interaction: Interaction; delivery: { channel?: string } | DeliveryState; duplicate?: boolean };
  };
  'interaction.answer_batch': {
    params: { items: { interaction: string; decision_rev: number }[]; decision: 'allow' | 'deny' };
    result: { results: BatchResult[] };
  };
  'git.status': { params: { pane: string }; result: GitStatus };
  /** With `file`: one file's diff (`rev` set for base/range); without (base/range): `GitRevFiles`. */
  'git.diff': { params: GitDiffParams; result: GitDiff & Partial<GitRevFiles> };
  'git.log': { params: { pane: string; base?: string; limit?: number }; result: GitLog };
  'fs.list': { params: { pane: string; path?: string }; result: FsList };
  'fs.read': { params: { pane: string; path: string }; result: FsRead };
  'worktree.list': { params: { pane: string } | { workspace: string }; result: { worktrees: Worktree[] } };
  /** Host directories for path pickers: `path` absolute or `~`; `prefix` filters names (a leading `.` shows dot-folders). */
  'fs.browse': { params: { path?: string; prefix?: string }; result: BrowseResult };
  /** Clones on the host whose remote is `origin` (workspaces and a shallow scan of ~/code, ~/src, …). */
  'repo.candidates': { params: { origin: string }; result: { repos: RepoCandidate[] } };
  /** `data_b64` is standard (padded) base64, as the server's image.upload expects. */
  'attachment.put': {
    params: { name: string; mime: string; data_b64: string };
    result: { path: string; hash?: string; size?: number };
  };
  'notification.list': { params: Record<string, unknown>; result: { notifications: Notification[] } };
  'notification.read': { params: Record<string, unknown>; result: unknown };
  'events.subscribe': { params: { after?: number }; result: { at: number; reset?: boolean } };
  /** `dnd_until` is unix seconds; 0 = off. */
  'prefs.get': { params: Record<string, never>; result: { device: DevicePrefs; host: { dnd_until?: number } } };
  'prefs.set': { params: { device?: DevicePrefs; host?: { dnd_until: number } }; result: unknown };
  'push.subscribe': {
    params: { subscription: { endpoint: string; keys: { p256dh: string; auth: string } }; vapid_private: string };
    result: Record<string, never>;
  };
  'push.unsubscribe': { params: { endpoint?: string }; result: Record<string, never> };
  'push.test': { params: Record<string, never>; result: { sent?: number } };
  /** `data_b64` is standard (padded) base64. */
  'stt.transcribe': { params: { mime: string; data_b64: string }; result: { text: string } };
  'devices.list': { params: Record<string, never>; result: { devices: DeviceInfo[] } };
  'devices.revoke': { params: { device: string }; result: unknown };
  ping: { params: Record<string, never>; result: Record<string, never> };
  /** Spec 16 §15: a scoped, expiring bearer pairing invitation (share) or a handoff invitation. */
  'share.create': {
    params: {
      kind: 'share' | 'handoff';
      scope?: 'view' | 'approve';
      ttl_s?: number;
      workspace?: string;
      pane?: string;
      /** Shown in the host's device list (`name` is accepted as an alias). */
      label?: string;
    };
    /** `open_by`: unix seconds the link must be opened by; `expires_at`: unix seconds access ends. */
    result: { link: string; pid: string; open_by?: number; expires_after_s?: number; expires_at?: number };
  };
  /** Spec 16 §15.2: incoming handoffs on this host (full-scope devices). */
  'handoff.incoming.list': { params: Record<string, never>; result: { incoming: IncomingHandoff[] } };
  'handoff.incoming.get': {
    params: { id: string };
    result: { incoming: IncomingHandoff; suggested: { repos: string[]; repo: string | null; worktree_path: string | null; branch: string } };
  };
  'handoff.accept': {
    params: {
      id: string;
      repo: { path: string } | { clone_to: string };
      worktree_path?: string;
      branch?: string;
      start_agent?: boolean;
      trust?: ('mise' | 'direnv')[];
    };
    result: { incoming: IncomingHandoff };
  };
  'handoff.decline': { params: { id: string }; result: { incoming: IncomingHandoff } };
  'handoff.resume': { params: { id: string }; result: { incoming: IncomingHandoff; run?: unknown; agent_error?: { code?: number; message?: string; data?: { kind?: string } } } };
  /** Read, or set whether the user's own handoffs always wait to be accepted. */
  'handoff.prefs': { params: { always_ask?: boolean }; result: HandoffPrefs };
  /** Spec 16 §15.3: invite another of the owner's hosts (link open for `ttl_s`, default 15 min). */
  'peer.invite': { params: { ttl_s?: number }; result: { link: string; pid: string; open_by: number } };
  /** This host redeems a peer or handoff invitation; `share_user` shows git user.name/email there. */
  'peer.redeem': { params: { link: string; share_user?: boolean }; result: { peer: PeerInfo } };
  'peer.list': { params: Record<string, never>; result: { peers: PeerInfo[] } };
  /** By id or name. */
  'peer.remove': { params: { id: string }; result: Record<string, never> };
  /** Spec 16 §15.4: pending invitations and the share, handoff and peer devices they produced. */
  'share.list': { params: Record<string, never>; result: { invitations: InvitationInfo[]; devices: InvitedDeviceInfo[] } };
  /** Cancel a pending invitation (pairing id) or revoke an invited device (device id). */
  'share.revoke': { params: { id: string }; result: { cancelled: 'invitation' | 'device' } };
  /** Spec 16 §15.2: hand a pane's work to a peer; the host's gateway exports and delivers it (the app may close). */
  'handoff.send': { params: { pane: string; peer: string; interrupt?: boolean }; result: { job: HandoffJob } };
  /** Outgoing handoffs, newest first (finished ones for 7 days). */
  'handoff.jobs': { params: Record<string, never>; result: { jobs: HandoffJob[] } };
  /** Stop a queued or running job; the destination drops what it received. */
  'handoff.cancel': { params: { id: string }; result: { job: HandoffJob } };
  /** The hosts `handoff.send` can deliver to, as this host's gateway last published them. */
  'handoff.peers': { params: Record<string, never>; result: { peers: HandoffPeer[]; updated_at: number | null } };
  /** Open approval requests from panes and the standing grants (the gateway passes only these parts of the host's `auth.list`). */
  'auth.list': { params: Record<string, never>; result: { approvals?: ApprovalRequest[]; grants?: ApprovalGrant[] } };
  /**
   * Decide a pane's request (full-scope devices): `approve` runs the frozen call once as the user,
   * `always` also allows the same call from that pane to that peer until the pane restarts (only
   * when `always_allowed`), `deny` refuses. `ok`/`result`/`error` are the approved call's outcome.
   */
  'auth.approve.decide': {
    params: { request: string; decision: ApprovalDecision };
    result: {
      request: string;
      pane: string;
      decision: 'approved' | 'denied';
      grant: 'once' | 'always' | null;
      ok: boolean;
      result: unknown;
      error: { code?: number; message?: string; data?: { kind?: string } } | null;
    };
  };
}

export type AppMethod = keyof AppApi;

/** Methods that carry `op_id` (spec 16 §7.3/§7.4); mirrors vk-gateway api::is_mutating. */
export const MUTATING_METHODS: ReadonlySet<string> = new Set([
  'pane.send_text',
  'pane.send_keys',
  'pane.rename',
  'pane.close',
  'pane.focus',
  'agent.prompt',
  'agent.interrupt',
  'agent.start',
  'tab.create',
  'tab.rename',
  'tab.close',
  'tab.focus',
  'preview.open',
  'interaction.answer',
  'interaction.answer_batch',
  'notification.read',
  'prefs.set',
  'devices.revoke',
  'attachment.put',
  'push.subscribe',
  'push.unsubscribe',
  'push.test',
  'stt.transcribe',
  'share.create',
  'peer.invite',
  'peer.redeem',
  'peer.remove',
  'share.revoke',
  'handoff.accept',
  'handoff.decline',
  'handoff.resume',
  'handoff.prefs',
  'handoff.send',
  'handoff.cancel',
  'auth.approve.decide',
]);

// ---- normalization ------------------------------------------------------------------------

/** `RateLimited` → `rate_limited`; already-snake strings pass through. */
export function snake(s: string): string {
  return s.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toLowerCase();
}

const snakeOr = <T>(v: unknown): T => (typeof v === 'string' ? (snake(v) as T) : (v as T));

export function normalizeInteraction(raw: unknown): Interaction {
  const i = { ...(raw as Record<string, unknown>) } as unknown as Interaction;
  i.kind = snakeOr(i.kind);
  i.status = snakeOr(i.status);
  i.delivery = snakeOr(i.delivery);
  i.source = snakeOr(i.source);
  i.answer_channel = snakeOr(i.answer_channel);
  if (i.action) i.action = { ...i.action, risk: snakeOr(i.action.risk) };
  if (i.answer) i.answer = { ...i.answer, decision: i.answer.decision === null ? null : snakeOr(i.answer.decision) };
  return i;
}

export function normalizeRun(raw: unknown): AgentRun {
  const r = { ...(raw as Record<string, unknown>) } as unknown as AgentRun;
  if (r.execution) r.execution = { ...r.execution, value: snakeOr(r.execution.value), source: snakeOr(r.execution.source) };
  r.health = snakeOr(r.health);
  return r;
}

export function normalizeDashboard(raw: unknown): Dashboard {
  const d = raw as Dashboard;
  return {
    ...d,
    runs: (d.runs ?? []).map(normalizeRun),
    interactions: (d.interactions ?? []).map(normalizeInteraction),
    workspaces: d.workspaces ?? [],
    tabs: d.tabs ?? [],
    panes: d.panes ?? [],
    tasks: d.tasks ?? [],
    ...(Array.isArray(d.previews) ? { previews: d.previews.map((p) => ({ ...p, status: snakeOr<string>(p.status) })) } : {}),
    notifications_unread: d.notifications_unread ?? 0,
  };
}
