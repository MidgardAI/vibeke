export const docManifest = [
  { slug: 'introduction', title: 'Introduction', group: 'Start here', file: 'introduction.md', description: 'Run and control your coding agents.' },
  { slug: 'install', title: 'Installation', group: 'Start here', file: 'install.md', description: 'Install the CLI, download the desktop app, or open the browser app.' },
  { slug: 'quickstart', title: 'Your first workspace', group: 'Start here', file: 'quickstart.md', description: 'Create a workspace, launch an agent, and reconnect.' },
  { slug: 'terminal', title: 'Using the terminal', group: 'Start here', file: 'terminal.md', description: 'Use the command palette, panes, setup, and attention inbox.' },
  { slug: 'layout', title: 'Workspaces & panes', group: 'Core concepts', file: 'concepts/layout.md', description: 'Understand sessions, workspaces, tabs, and panes.' },
  { slug: 'agents', title: 'Agents & interactions', group: 'Core concepts', file: 'concepts/agents.md', description: 'Read agent state. Answer requests through an integration.' },
  { slug: 'holders', title: 'Process durability', group: 'Core concepts', file: 'concepts/holders.md', description: 'Keep agent processes active after a server restart.' },
  { slug: 'tasks', title: 'Tasks & review', group: 'Core concepts', file: 'concepts/tasks.md', description: 'Use separate worktrees. Check and review the results.' },
  { slug: 'previews', title: 'Remote & previews', group: 'Core concepts', file: 'concepts/previews.md', description: 'Open remote previews and browser panes. Capture screenshots.' },
  { slug: 'sandboxes', title: 'Execution & isolation', group: 'Core concepts', file: 'concepts/sandboxes.md', description: 'Understand host, sandbox, container, and VM boundaries.' },
  { slug: 'mobile', title: 'Phone & browser', group: 'Guides', file: 'mobile.md', description: 'Pair a device through a relay. Control agents from another device.' },
  { slug: 'desktop', title: 'Desktop app', group: 'Guides', file: 'desktop.md', description: 'Download the app and connect to a local or remote host.' },
  { slug: 'remote', title: 'Connect through SSH', group: 'Guides', file: 'remote.md', description: 'Install a signed release on your host and connect through SSH.' },
  { slug: 'handoff', title: 'Transfers & shared access', group: 'Guides', file: 'handoff.md', description: 'Transfer work to another host or teammate. Share access to a pane.' },
  { slug: 'cloud-sandboxes', title: 'Cloud sandboxes', group: 'Guides', file: 'cloud-sandboxes.md', description: 'Run agents on a cloud machine. Send work there and bring it back.' },
  { slug: 'migrating-from-herdr', title: 'Moving from Herdr', group: 'Guides', file: 'migrating-from-herdr.md', description: 'Import supported configuration and layouts from Herdr.' },
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
