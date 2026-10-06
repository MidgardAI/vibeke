export const docManifest = [
  { slug: 'introduction', title: 'Introduction', group: 'Start here', file: 'introduction.md', description: 'Run and control your coding agents.' },
  { slug: 'install', title: 'Installation', group: 'Start here', file: 'install.md', description: 'Build from source or install a release. Check your setup.' },
  { slug: 'quickstart', title: 'Your first workspace', group: 'Start here', file: 'quickstart.md', description: 'Create a workspace, launch an agent, and reconnect.' },
  { slug: 'layout', title: 'Workspaces & panes', group: 'Core concepts', file: 'concepts/layout.md', description: 'Understand sessions, workspaces, tabs, and panes.' },
  { slug: 'agents', title: 'Agents & interactions', group: 'Core concepts', file: 'concepts/agents.md', description: 'Read agent state. Answer requests through an integration.' },
  { slug: 'holders', title: 'Process durability', group: 'Core concepts', file: 'concepts/holders.md', description: 'Keep agent processes active after a server restart.' },
  { slug: 'tasks', title: 'Tasks & review', group: 'Core concepts', file: 'concepts/tasks.md', description: 'Use separate worktrees. Check and review the results.' },
  { slug: 'previews', title: 'Remote & previews', group: 'Core concepts', file: 'concepts/previews.md', description: 'Open remote previews and browser panes. Capture screenshots.' },
  { slug: 'sandboxes', title: 'Execution & isolation', group: 'Core concepts', file: 'concepts/sandboxes.md', description: 'Understand host, sandbox, container, and VM boundaries.' },
  { slug: 'mobile', title: 'Mobile & desktop', group: 'Guides', file: 'mobile.md', description: 'Pair a device through a relay. Control agents from another device.' },
  { slug: 'handoff', title: 'Transfers & shared access', group: 'Guides', file: 'handoff.md', description: 'Transfer work to another host or teammate. Share access to a pane.' },
  { slug: 'migrating-from-herdr', title: 'Moving from Herdr', group: 'Guides', file: 'migrating-from-herdr.md', description: 'Import supported configuration and layouts from Herdr.' },
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
