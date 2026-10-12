export const docManifest = [
  { slug: 'introduction', title: 'Introduction', group: 'Start here', file: 'introduction.md', description: 'Run and control your coding agents.' },
  { slug: 'install', title: 'Installation', group: 'Start here', file: 'install.md', description: 'Install the CLI, download the desktop app, or open the browser app.' },
  { slug: 'quickstart', title: 'Your first workspace', group: 'Start here', file: 'quickstart.md', description: 'Create a workspace, launch an agent, and reconnect.' },
  { slug: 'first-task', title: 'Your first task', group: 'Start here', file: 'first-task.md', description: 'Give an agent its own worktree. Answer it, review the result, and clean up.' },
  { slug: 'phone-quickstart', title: 'Answer from your phone', group: 'Start here', file: 'phone-quickstart.md', description: 'Pair a phone and answer an agent request away from your desk.' },
  { slug: 'terminal', title: 'Using the terminal', group: 'Start here', file: 'terminal.md', description: 'Use the command palette, panes, setup, and attention inbox.' },
  { slug: 'parallel-agents', title: 'Run agents in parallel', group: 'Guides', file: 'parallel-agents.md', description: 'Run several agents on one repository without conflicts.' },
  { slug: 'customize', title: 'Customize keys & appearance', group: 'Guides', file: 'customize.md', description: 'Change the prefix key, bindings, copy mode, theme, and notifications.' },
  { slug: 'remote', title: 'Work on a remote machine', group: 'Guides', file: 'remote.md', description: 'Install on a host through SSH. Connect from the terminal, desktop, or phone.' },
  { slug: 'desktop', title: 'Desktop app', group: 'Guides', file: 'desktop.md', description: 'Download the app and connect to a local or remote host.' },
  { slug: 'mobile', title: 'Phone & browser', group: 'Guides', file: 'mobile.md', description: 'Pair a device through a relay. Control agents from another device.' },
  { slug: 'handoff', title: 'Move work to another host', group: 'Guides', file: 'handoff.md', description: 'Transfer work between your own paired hosts.' },
  { slug: 'sharing', title: 'Work with a teammate', group: 'Guides', file: 'sharing.md', description: 'Transfer work to a teammate. Share access to a pane or workspace.' },
  { slug: 'cloud-sandboxes', title: 'Cloud sandboxes', group: 'Guides', file: 'cloud-sandboxes.md', description: 'Run agents on a cloud machine. Send work there and bring it back.' },
  { slug: 'scripting', title: 'Script & automate', group: 'Guides', file: 'scripting.md', description: 'Control Vibeke from scripts, the CLI, and the Python and TypeScript clients.' },
  { slug: 'migrating-from-herdr', title: 'Moving from Herdr', group: 'Guides', file: 'migrating-from-herdr.md', description: 'Import supported configuration and layouts from Herdr.' },
  { slug: 'claude-code', title: 'Claude Code', group: 'Agents', file: 'agents/claude-code.md', description: 'Install the Claude Code integration. See what it reports and answers.' },
  { slug: 'codex', title: 'Codex CLI', group: 'Agents', file: 'agents/codex.md', description: 'Install the Codex integration. See what it reports and answers.' },
  { slug: 'pi', title: 'pi & oh-my-pi', group: 'Agents', file: 'agents/pi.md', description: 'Install the pi and oh-my-pi extensions. See what they report and answer.' },
  { slug: 'opencode', title: 'OpenCode', group: 'Agents', file: 'agents/opencode.md', description: 'Install the OpenCode plugin. See what it reports and answers.' },
  { slug: 'hermes', title: 'Hermes Agent', group: 'Agents', file: 'agents/hermes.md', description: 'Install the Hermes Agent plugin. See what it reports and answers.' },
  { slug: 'other-agents', title: 'Other agents', group: 'Agents', file: 'agents/other.md', description: 'Use any terminal agent, custom manifests, or ACP agents.' },
  { slug: 'layout', title: 'Workspaces & panes', group: 'Core concepts', file: 'concepts/layout.md', description: 'Understand sessions, workspaces, tabs, and panes.' },
  { slug: 'agents', title: 'Agents & interactions', group: 'Core concepts', file: 'concepts/agents.md', description: 'Read agent state. Answer requests through an integration.' },
  { slug: 'holders', title: 'Process durability', group: 'Core concepts', file: 'concepts/holders.md', description: 'Keep agent processes active after a server restart.' },
  { slug: 'tasks', title: 'Tasks & review', group: 'Core concepts', file: 'concepts/tasks.md', description: 'Use separate worktrees. Check and review the results.' },
  { slug: 'previews', title: 'Remote & previews', group: 'Core concepts', file: 'concepts/previews.md', description: 'Open remote previews and browser panes. Capture screenshots.' },
  { slug: 'sandboxes', title: 'Execution & isolation', group: 'Core concepts', file: 'concepts/sandboxes.md', description: 'Understand host, sandbox, container, and VM boundaries.' },
  { slug: 'troubleshooting', title: 'Troubleshooting', group: 'Help', file: 'troubleshooting.md', description: 'Find the cause of common problems and collect a debug bundle.' },
  { slug: 'faq', title: 'FAQ', group: 'Help', file: 'faq.md', description: 'Short answers to common questions about Vibeke.' },
  { slug: 'self-hosting', title: 'Self-hosting', group: 'Advanced', file: 'self-hosting.md', description: 'Operate your own relay or browser app.' },
  { slug: 'development', title: 'Build from source', group: 'Advanced', file: 'development.md', description: 'Build the CLI and apps for development.' },
  { slug: 'cli', title: 'CLI reference', group: 'Reference', file: 'reference/cli.md', description: 'Every command, flag, and corresponding API method.' },
  { slug: 'config', title: 'Configuration', group: 'Reference', file: 'reference/config.md', description: 'The complete configuration, generated from code.' },
  { slug: 'api', title: 'Control API', group: 'Reference', file: 'reference/api.md', description: 'Call methods. Check access, parameters, and results.' },
  { slug: 'security', title: 'Security model', group: 'Reference', file: 'security.md', description: 'Trust boundaries, permissions, data, and release verification.' },
  { slug: 'releases', title: 'Releases & hardening', group: 'Reference', file: 'reference/releases.md', description: 'Check release files and build results. Read current limits.' },
] as const

export interface Doc {
  slug: string
  title: string
  group: string
  file: string
  description: string
  body: string
  headings: { id: string; text: string; depth: number }[]
}
