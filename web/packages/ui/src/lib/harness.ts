// Per-harness static data: slash-command palettes and default quick replies (spec 16 §9.1).
// A slash command is just text: no-arg commands are sent with submit; commands that take an
// argument are inserted into the composer to complete.

export interface SlashCommand {
  command: string;
  description: string;
  takesArg?: boolean;
  /** Disruptive enough to ask twice (wipes context, logs out). */
  dangerous?: boolean;
}

const CLAUDE: SlashCommand[] = [
  { command: '/compact', description: 'Summarize the conversation to free context', takesArg: true },
  { command: '/clear', description: 'Start a fresh conversation', dangerous: true },
  { command: '/cost', description: 'Token cost and usage of this session' },
  { command: '/model', description: 'Switch the model', takesArg: true },
  { command: '/context', description: 'Context window usage' },
  { command: '/status', description: 'Version, model, account' },
  { command: '/review', description: 'Review a pull request', takesArg: true },
  { command: '/init', description: 'Generate a starter CLAUDE.md' },
  { command: '/memory', description: 'Edit memory files' },
  { command: '/permissions', description: 'Allow / ask / deny rules' },
  { command: '/resume', description: 'Resume a previous conversation', takesArg: true },
  { command: '/plan', description: 'Enter plan mode', takesArg: true },
  { command: '/help', description: 'List commands' },
];

const CODEX: SlashCommand[] = [
  { command: '/status', description: 'Session configuration and token usage' },
  { command: '/diff', description: 'Show the git diff, including untracked files' },
  { command: '/review', description: 'Review current changes' },
  { command: '/compact', description: 'Summarize to free context' },
  { command: '/model', description: 'Choose model and reasoning effort' },
  { command: '/approvals', description: 'Choose what Codex may do without asking' },
  { command: '/new', description: 'Start a new chat', dangerous: true },
  { command: '/init', description: 'Create an AGENTS.md' },
  { command: '/mention', description: 'Mention a file', takesArg: true },
  { command: '/mcp', description: 'List MCP tools' },
];

const PI: SlashCommand[] = [
  { command: '/model', description: 'Switch model', takesArg: true },
  { command: '/compact', description: 'Compact the session', takesArg: true },
  { command: '/session', description: 'Session info' },
  { command: '/new', description: 'New session', dangerous: true },
  { command: '/tree', description: 'Session tree' },
  { command: '/copy', description: 'Copy last reply' },
];

export const SLASH_COMMANDS: Record<string, SlashCommand[]> = {
  claude: CLAUDE,
  codex: CODEX,
  pi: PI,
  omp: PI,
};

export const slashCommandsFor = (harness: string | null | undefined): SlashCommand[] =>
  (harness && SLASH_COMMANDS[harness.toLowerCase()]) || [];

export const DEFAULT_QUICK_REPLIES: Record<string, string[]> = {
  '*': ['continue', 'yes', 'run the tests', 'commit it'],
  claude: ['continue', 'yes', 'run the tests', 'commit it'],
  codex: ['continue', 'yes', 'run the tests', 'commit it'],
};

export function quickRepliesFor(harness: string | null | undefined, custom: Record<string, string[]>): string[] {
  const key = harness?.toLowerCase() ?? '*';
  return custom[key] ?? custom['*'] ?? DEFAULT_QUICK_REPLIES[key] ?? DEFAULT_QUICK_REPLIES['*']!;
}

export const harnessLabel = (h: string | null | undefined): string => {
  if (!h) return '';
  const known: Record<string, string> = { claude: 'Claude', codex: 'Codex', pi: 'pi', omp: 'omp' };
  return known[h.toLowerCase()] ?? h;
};
