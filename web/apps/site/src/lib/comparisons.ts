export const comparisonDate = 'October 6, 2026'

export const products = [
  {
    "id": "vibeke",
    "name": "Vibeke",
    "kind": "terminal",
    "source": "/docs/introduction",
    "note": "Terminal workspace"
  },
  {
    "id": "herdr",
    "name": "Herdr",
    "kind": "terminal",
    "source": "https://herdr.dev/docs/",
    "note": "Terminal workspace"
  },
  {
    "id": "cmux",
    "name": "cmux",
    "kind": "terminal",
    "source": "https://cmux.com/",
    "note": "macOS terminal"
  },
  {
    "id": "warp",
    "name": "Warp",
    "kind": "app",
    "source": "https://docs.warp.dev/",
    "note": "Terminal and cloud"
  },
  {
    "id": "conductor",
    "name": "Conductor",
    "kind": "app",
    "source": "https://www.conductor.build/docs",
    "note": "Local and cloud"
  },
  {
    "id": "zellij",
    "name": "Zellij",
    "kind": "terminal",
    "source": "https://zellij.dev/documentation/",
    "note": "Terminal workspace"
  },
  {
    "id": "tmux",
    "name": "tmux",
    "kind": "terminal",
    "source": "https://github.com/tmux/tmux/wiki",
    "note": "Terminal multiplexer"
  },
  {
    "id": "solo",
    "name": "Solo",
    "kind": "app",
    "source": "https://soloterm.com/docs",
    "note": "Local agent app"
  },
  {
    "id": "emdash",
    "name": "Emdash",
    "kind": "app",
    "source": "https://emdash.com/docs",
    "note": "Local and SSH"
  },
  {
    "id": "superset",
    "name": "Superset",
    "kind": "app",
    "source": "https://docs.superset.sh/overview",
    "note": "App and host service"
  }
] as const
export type Product = (typeof products)[number]
export type ProductId = Product['id']
export interface ComparisonCell {
  label: string
  detail: string
  source: string
  state: 'yes' | 'no' | 'partial' | 'text'
}
export interface ComparisonRow {
  id: string
  label: string
  category: string
  featured: boolean
  cells: Record<ProductId, ComparisonCell>
}

// Compare built-in features. Keep process survival, session restore, and host transfer distinct.
// Short labels are visible by default. Row details retain qualifications and official sources.
export const comparisonRows: ComparisonRow[] = [
  {
    "id": "client-disconnect",
    "label": "Client disconnect",
    "category": "runtime",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Pane processes continue.",
        "source": "/docs/holders",
        "state": "yes"
      },
      "herdr": {
        "label": "Yes",
        "detail": "Pane processes continue.",
        "source": "https://herdr.dev/docs/session-state/",
        "state": "yes"
      },
      "cmux": {
        "label": "Remote only",
        "detail": "Remote sessions continue. Local app restore is separate.",
        "source": "https://cmux.com/docs/ssh",
        "state": "partial"
      },
      "warp": {
        "label": "Cloud only",
        "detail": "Cloud runs continue.",
        "source": "https://docs.warp.dev/platform/quickstart/",
        "state": "partial"
      },
      "conductor": {
        "label": "Cloud only",
        "detail": "Cloud runs continue. Local sessions end when the app closes.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "partial"
      },
      "zellij": {
        "label": "Yes",
        "detail": "Server sessions continue.",
        "source": "https://zellij.dev/documentation/session-resurrection.html",
        "state": "yes"
      },
      "tmux": {
        "label": "Yes",
        "detail": "Detach without stopping terminal processes.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "yes"
      },
      "solo": {
        "label": "Window only",
        "detail": "Closing a window keeps processes. Quitting Solo stops them.",
        "source": "https://soloterm.com/",
        "state": "partial"
      },
      "emdash": {
        "label": "With tmux",
        "detail": "Enable tmux to keep processes after disconnects.",
        "source": "https://emdash.com/docs/tmux-sessions",
        "state": "partial"
      },
      "superset": {
        "label": "Yes",
        "detail": "Terminal processes continue through app restarts.",
        "source": "https://docs.superset.sh/terminal-integration",
        "state": "yes"
      }
    },
    "featured": true
  },
  {
    "id": "server-or-app-restart",
    "label": "Server or app restart",
    "category": "runtime",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Separate holder processes keep pane processes active.",
        "source": "/docs/holders",
        "state": "yes"
      },
      "herdr": {
        "label": "Partial",
        "detail": "Restart ends processes. Experimental live handoff can preserve them during supported updates.",
        "source": "https://herdr.dev/docs/session-state/",
        "state": "partial"
      },
      "cmux": {
        "label": "Restore",
        "detail": "Restores layouts, scrollback, and agent sessions.",
        "source": "https://cmux.com/docs/session-restore",
        "state": "partial"
      },
      "warp": {
        "label": "Cloud only",
        "detail": "Cloud execution is separate from the client.",
        "source": "https://docs.warp.dev/platform/quickstart/",
        "state": "partial"
      },
      "conductor": {
        "label": "Cloud only",
        "detail": "Cloud execution is separate from the client.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "partial"
      },
      "zellij": {
        "label": "Restore",
        "detail": "Restores layouts and commands. Confirm before commands start.",
        "source": "https://zellij.dev/documentation/session-resurrection.html",
        "state": "partial"
      },
      "tmux": {
        "label": "No",
        "detail": "Stopping the tmux server ends its sessions.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "An explicit quit or update stops processes. Crash recovery can adopt surviving processes.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "With tmux",
        "detail": "App restart reconnects to tmux. The tmux server must stay active.",
        "source": "https://emdash.com/docs/tmux-sessions",
        "state": "partial"
      },
      "superset": {
        "label": "App only",
        "detail": "App restarts preserve processes. A host-service restart interrupts terminals.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "partial"
      }
    },
    "featured": true
  },
  {
    "id": "agent-status",
    "label": "Agent status",
    "category": "agents",
    "cells": {
      "vibeke": {
        "label": "Native + screen",
        "detail": "Native events or screen detection. Shows the source and confidence.",
        "source": "/docs/agents",
        "state": "text"
      },
      "herdr": {
        "label": "Native + screen",
        "detail": "Screen detection and native state reports.",
        "source": "https://herdr.dev/docs/agents/",
        "state": "text"
      },
      "cmux": {
        "label": "Hooks",
        "detail": "Pane indicators and hook notifications.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Yes",
        "detail": "Agent panel and notifications.",
        "source": "https://docs.warp.dev/agents/capabilities/agent-notifications/",
        "state": "yes"
      },
      "conductor": {
        "label": "Yes",
        "detail": "Workspace and agent activity.",
        "source": "https://www.conductor.build/docs/concepts/workflow",
        "state": "yes"
      },
      "zellij": {
        "label": "Plugins",
        "detail": "Terminal output. Plugins can add status.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "no"
      },
      "tmux": {
        "label": "Scripts",
        "detail": "Terminal activity. Add agent state with scripts.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "Screen detection",
        "detail": "Heuristics detect work, idle, permission requests, and errors.",
        "source": "https://soloterm.com/",
        "state": "text"
      },
      "emdash": {
        "label": "Provider state",
        "detail": "Agent activity appears in each task.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Yes",
        "detail": "Agent state and notifications.",
        "source": "https://docs.superset.sh/agent-status",
        "state": "yes"
      }
    },
    "featured": true
  },
  {
    "id": "approvals-and-questions",
    "label": "Approvals and questions",
    "category": "agents",
    "cells": {
      "vibeke": {
        "label": "Inbox",
        "detail": "Answer requests from the inbox. The integration sends the answer.",
        "source": "/docs/agents",
        "state": "text"
      },
      "herdr": {
        "label": "Agent pane",
        "detail": "Answer in the agent pane.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "text"
      },
      "cmux": {
        "label": "Agent pane",
        "detail": "Answer in the agent pane.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Agent prompts",
        "detail": "Permission prompts and agent notifications.",
        "source": "https://docs.warp.dev/agents/capabilities/agent-notifications/",
        "state": "text"
      },
      "conductor": {
        "label": "Agent prompts",
        "detail": "Agent permission and question prompts.",
        "source": "https://www.conductor.build/changelog/0.63.0-cursor-support-dispatcher",
        "state": "text"
      },
      "zellij": {
        "label": "Agent pane",
        "detail": "Answer in the agent pane.",
        "source": "https://zellij.dev/documentation/",
        "state": "text"
      },
      "tmux": {
        "label": "Agent pane",
        "detail": "Answer inside the agent terminal.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "Agent pane",
        "detail": "Use the agent terminal for permission requests.",
        "source": "https://soloterm.com/docs/agents",
        "state": "text"
      },
      "emdash": {
        "label": "Agent prompts",
        "detail": "Answer through the selected agent interface.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Agent prompts",
        "detail": "Chat includes questions, approvals, and plan review.",
        "source": "https://docs.superset.sh/agent-integration",
        "state": "text"
      }
    },
    "featured": true
  },
  {
    "id": "move-work-between-hosts",
    "label": "Move work between hosts",
    "category": "handoff",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Transfer work to another paired host after the agent completes a turn.",
        "source": "/docs/handoff",
        "state": "yes"
      },
      "herdr": {
        "label": "No",
        "detail": "No transfer between hosts.",
        "source": "https://herdr.dev/blog/connecting-the-machines/",
        "state": "no"
      },
      "cmux": {
        "label": "No",
        "detail": "No transfer between hosts. Connect to the original host through SSH.",
        "source": "https://cmux.com/docs/ssh",
        "state": "no"
      },
      "warp": {
        "label": "Partial",
        "detail": "Local-to-cloud transfer supports Warp Agent. Cloud-to-cloud continuation also supports Claude Code and Codex.",
        "source": "https://docs.warp.dev/platform/handoff/",
        "state": "partial"
      },
      "conductor": {
        "label": "No",
        "detail": "No transfer between hosts. Connect to the same cloud workspace from another device.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "no"
      },
      "zellij": {
        "label": "No",
        "detail": "No transfer between hosts. Connect to the original session host.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "no"
      },
      "tmux": {
        "label": "No",
        "detail": "SSH reconnects to the original host.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "No remote-host transfer.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "No",
        "detail": "Remote projects run on the selected SSH host.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "no"
      },
      "superset": {
        "label": "No",
        "detail": "Remote access connects to the original workspace host.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "no"
      }
    },
    "featured": true
  },
  {
    "id": "transfer-work-to-a-teammate",
    "label": "Transfer work to a teammate",
    "category": "handoff",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Import work on their host. They decide when to start the agent.",
        "source": "/docs/handoff",
        "state": "yes"
      },
      "herdr": {
        "label": "No",
        "detail": "No.",
        "source": "https://herdr.dev/docs/connecting-machines/",
        "state": "no"
      },
      "cmux": {
        "label": "No",
        "detail": "No transfer to a teammate’s host. Use Git and external tools.",
        "source": "https://cmux.com/",
        "state": "no"
      },
      "warp": {
        "label": "Shared run",
        "detail": "Share a session with view or edit access.",
        "source": "https://docs.warp.dev/agents/cli/oz-cli/",
        "state": "text"
      },
      "conductor": {
        "label": "Shared run",
        "detail": "Share a cloud workspace link.",
        "source": "https://www.conductor.build/",
        "state": "text"
      },
      "zellij": {
        "label": "Same host",
        "detail": "Share access to the original session.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "text"
      },
      "tmux": {
        "label": "Same host",
        "detail": "Share access to the original host session.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "No",
        "detail": "Project commands can be shared. Running agents stay local.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "Use Git",
        "detail": "Share changes through Git and pull requests.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "no"
      },
      "superset": {
        "label": "Shared host",
        "detail": "Grant a teammate access to the workspace host.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "text"
      }
    },
    "featured": true
  },
  {
    "id": "browser-pane",
    "label": "Browser pane",
    "category": "browser",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Chromium pane beside the terminal.",
        "source": "/docs/previews",
        "state": "yes"
      },
      "herdr": {
        "label": "No",
        "detail": "No.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "no"
      },
      "cmux": {
        "label": "Yes",
        "detail": "Browser pane beside the terminal.",
        "source": "https://cmux.com/docs/browser-automation",
        "state": "yes"
      },
      "warp": {
        "label": "Cloud only",
        "detail": "Chromium in Computer Use environments.",
        "source": "https://docs.warp.dev/agents/capabilities/computer-use/browser-use/",
        "state": "partial"
      },
      "conductor": {
        "label": "Yes",
        "detail": "In-app browser preview.",
        "source": "https://www.conductor.build/changelog/page/5",
        "state": "yes"
      },
      "zellij": {
        "label": "No",
        "detail": "No. Use an external browser.",
        "source": "https://zellij.dev/documentation/",
        "state": "no"
      },
      "tmux": {
        "label": "No",
        "detail": "Use an external browser.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "Opens a configured external browser.",
        "source": "https://soloterm.com/docs/settings/tools-editors-terminals",
        "state": "no"
      },
      "emdash": {
        "label": "Yes",
        "detail": "Browser tabs open inside a task.",
        "source": "https://emdash.com/docs/in-app-browser",
        "state": "yes"
      },
      "superset": {
        "label": "Yes",
        "detail": "Embedded browser panes beside terminals.",
        "source": "https://docs.superset.sh/browser",
        "state": "yes"
      }
    },
    "featured": true
  },
  {
    "id": "agent-browser-control",
    "label": "Agent browser control",
    "category": "browser",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Navigate, click, type, and inspect the page through the API.",
        "source": "/docs/previews",
        "state": "yes"
      },
      "herdr": {
        "label": "No",
        "detail": "No.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "no"
      },
      "cmux": {
        "label": "Yes",
        "detail": "Browser commands through the CLI and socket API.",
        "source": "https://cmux.com/docs/browser-automation",
        "state": "yes"
      },
      "warp": {
        "label": "Yes",
        "detail": "Playwright CLI and visual browser tools.",
        "source": "https://docs.warp.dev/agents/capabilities/computer-use/browser-use/",
        "state": "yes"
      },
      "conductor": {
        "label": "Agent tools",
        "detail": "Agent tools can use Chrome in cloud sandboxes.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "text"
      },
      "zellij": {
        "label": "No",
        "detail": "Use external tools.",
        "source": "https://zellij.dev/documentation/",
        "state": "no"
      },
      "tmux": {
        "label": "No",
        "detail": "Use external browser tools.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "Agent tools",
        "detail": "Use agent-provided browser tools.",
        "source": "https://soloterm.com/docs/agents",
        "state": "text"
      },
      "emdash": {
        "label": "Agent tools",
        "detail": "Use browser tools from the agent or MCP setup.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Yes",
        "detail": "CLI commands and CDP control the browser pane.",
        "source": "https://docs.superset.sh/browser",
        "state": "yes"
      }
    },
    "featured": true
  },
  {
    "id": "git-worktrees",
    "label": "Git worktrees",
    "category": "tasks",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Task worktrees with separate branches, ports, and setup.",
        "source": "/docs/tasks",
        "state": "yes"
      },
      "herdr": {
        "label": "Yes",
        "detail": "Create, open, list, and remove worktrees.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "yes"
      },
      "cmux": {
        "label": "Use Git",
        "detail": "Use Git or agent commands.",
        "source": "https://cmux.com/",
        "state": "no"
      },
      "warp": {
        "label": "Yes",
        "detail": "Worktree creation and tab configurations.",
        "source": "https://docs.warp.dev/changelog/2026/",
        "state": "yes"
      },
      "conductor": {
        "label": "Yes",
        "detail": "A worktree and branch for each local workspace.",
        "source": "https://www.conductor.build/docs/concepts/git-worktrees",
        "state": "yes"
      },
      "zellij": {
        "label": "Use Git",
        "detail": "Use Git or plugins.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "no"
      },
      "tmux": {
        "label": "Use Git",
        "detail": "Create worktrees with Git commands.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "Linked checkouts",
        "detail": "Links existing worktrees and clones.",
        "source": "https://soloterm.com/",
        "state": "text"
      },
      "emdash": {
        "label": "Yes",
        "detail": "Creates task worktrees locally or over SSH.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "yes"
      },
      "superset": {
        "label": "Yes",
        "detail": "Each task can use its own worktree.",
        "source": "https://docs.superset.sh/workspaces",
        "state": "yes"
      }
    },
    "featured": true
  },
  {
    "id": "interface",
    "label": "Interface",
    "category": "overview",
    "cells": {
      "vibeke": {
        "label": "TUI + apps",
        "detail": "Terminal, browser, and desktop app.",
        "source": "/docs/introduction",
        "state": "text"
      },
      "herdr": {
        "label": "TUI",
        "detail": "Terminal.",
        "source": "https://herdr.dev/docs/",
        "state": "text"
      },
      "cmux": {
        "label": "Desktop + iOS",
        "detail": "macOS app. iOS app in beta.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Terminal + web",
        "detail": "Desktop terminal, CLI, and web app.",
        "source": "https://docs.warp.dev/",
        "state": "text"
      },
      "conductor": {
        "label": "Desktop + iOS",
        "detail": "macOS and iOS apps.",
        "source": "https://www.conductor.build/docs",
        "state": "text"
      },
      "zellij": {
        "label": "TUI + web",
        "detail": "Terminal and web app.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "text"
      },
      "tmux": {
        "label": "TUI",
        "detail": "Runs inside an existing terminal.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "Desktop",
        "detail": "Native desktop app.",
        "source": "https://soloterm.com/docs",
        "state": "text"
      },
      "emdash": {
        "label": "Desktop",
        "detail": "Desktop app for macOS, Windows, and Linux.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Desktop + iOS",
        "detail": "Desktop app, iPhone app, and CLI.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "agent-support",
    "label": "Agent support",
    "category": "overview",
    "cells": {
      "vibeke": {
        "label": "Multiple CLIs",
        "detail": "Claude Code, Codex, pi/omp, and other agent CLIs. Integration support varies.",
        "source": "/docs/agents",
        "state": "text"
      },
      "herdr": {
        "label": "Multiple CLIs",
        "detail": "Agent detection and integrations for multiple CLIs.",
        "source": "https://herdr.dev/docs/agents/",
        "state": "text"
      },
      "cmux": {
        "label": "Any CLI",
        "detail": "Any terminal agent. Hooks add notifications.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Multiple CLIs",
        "detail": "Warp Agent and third-party agent CLIs.",
        "source": "https://docs.warp.dev/",
        "state": "text"
      },
      "conductor": {
        "label": "Multiple CLIs",
        "detail": "Claude Code, Codex, Cursor, and OpenCode.",
        "source": "https://www.conductor.build/docs",
        "state": "text"
      },
      "zellij": {
        "label": "Any CLI",
        "detail": "Any terminal agent.",
        "source": "https://zellij.dev/documentation/",
        "state": "text"
      },
      "tmux": {
        "label": "Any CLI",
        "detail": "Runs terminal programs, including agent CLIs.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "Multiple CLIs",
        "detail": "Runs installed agent CLIs and custom tools.",
        "source": "https://soloterm.com/docs/agents",
        "state": "text"
      },
      "emdash": {
        "label": "Multiple CLIs",
        "detail": "Supports multiple agent providers per task.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Multiple CLIs",
        "detail": "Runs terminal agents and built-in chat.",
        "source": "https://docs.superset.sh/agent-integration",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "execution-host",
    "label": "Execution host",
    "category": "overview",
    "cells": {
      "vibeke": {
        "label": "Local / SSH",
        "detail": "Your computer or an SSH host.",
        "source": "/docs/sandboxes",
        "state": "text"
      },
      "herdr": {
        "label": "Local / SSH",
        "detail": "Your computer or an SSH host.",
        "source": "https://herdr.dev/docs/",
        "state": "text"
      },
      "cmux": {
        "label": "Local / cloud",
        "detail": "Local computer, SSH hosts, and optional cloud VMs.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Local / cloud",
        "detail": "Local computer, your hosts, or cloud environments.",
        "source": "https://docs.warp.dev/platform/quickstart/",
        "state": "text"
      },
      "conductor": {
        "label": "Local / cloud",
        "detail": "Local Mac or managed cloud sandbox.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "text"
      },
      "zellij": {
        "label": "Your host",
        "detail": "The computer that runs the session.",
        "source": "https://zellij.dev/documentation/",
        "state": "text"
      },
      "tmux": {
        "label": "Your host",
        "detail": "Processes run on the tmux server host.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "Local / WSL",
        "detail": "Local execution. Windows projects can use WSL.",
        "source": "https://soloterm.com/",
        "state": "text"
      },
      "emdash": {
        "label": "Local / SSH",
        "detail": "Code and agents run locally or on an SSH host.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "text"
      },
      "superset": {
        "label": "Your hosts",
        "detail": "Local or remote host service.",
        "source": "https://docs.superset.sh/cli/host-server",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "remote-access",
    "label": "Remote access",
    "category": "handoff",
    "cells": {
      "vibeke": {
        "label": "SSH",
        "detail": "SSH hosts in one workspace.",
        "source": "/docs/cli",
        "state": "text"
      },
      "herdr": {
        "label": "SSH",
        "detail": "Local and saved SSH hosts in one client.",
        "source": "https://herdr.dev/docs/connecting-machines/",
        "state": "text"
      },
      "cmux": {
        "label": "SSH",
        "detail": "SSH workspaces with automatic reconnect.",
        "source": "https://cmux.com/docs/ssh",
        "state": "text"
      },
      "warp": {
        "label": "SSH / cloud",
        "detail": "SSH terminal sessions and remote agent sessions.",
        "source": "https://docs.warp.dev/",
        "state": "text"
      },
      "conductor": {
        "label": "Cloud",
        "detail": "Managed cloud workspaces.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "text"
      },
      "zellij": {
        "label": "SSH / HTTPS",
        "detail": "SSH or direct HTTPS attach.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "text"
      },
      "tmux": {
        "label": "SSH",
        "detail": "Attach to the remote tmux server over SSH.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "No",
        "detail": "No remote-host session management.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "SSH",
        "detail": "Remote projects use SSH connections.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "text"
      },
      "superset": {
        "label": "Relay",
        "detail": "Connect to registered hosts through the relay.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "transfer-files-and-conversation",
    "label": "Transfer files and conversation",
    "category": "handoff",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Git commits, file changes, and available Claude/Codex transcripts.",
        "source": "/docs/handoff",
        "state": "yes"
      },
      "herdr": {
        "label": "No",
        "detail": "No transfer between hosts.",
        "source": "https://herdr.dev/docs/session-state/",
        "state": "no"
      },
      "cmux": {
        "label": "No",
        "detail": "No transfer between hosts. Session restore on the original Mac.",
        "source": "https://cmux.com/docs/session-restore",
        "state": "no"
      },
      "warp": {
        "label": "Partial",
        "detail": "Local-to-cloud and cloud-to-cloud carry files and conversation. Cloud-to-local carries conversation but does not apply workspace patches.",
        "source": "https://docs.warp.dev/platform/handoff/",
        "state": "partial"
      },
      "conductor": {
        "label": "No",
        "detail": "No transfer between hosts. Files and chat remain in the cloud workspace.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "no"
      },
      "zellij": {
        "label": "No",
        "detail": "No transfer between hosts. Layout and command restore on the session host.",
        "source": "https://zellij.dev/documentation/session-resurrection.html",
        "state": "no"
      },
      "tmux": {
        "label": "No",
        "detail": "Use Git and external transfer tools.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "Shared command files exclude agent sessions.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "No",
        "detail": "Use Git for transfers between hosts.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "no"
      },
      "superset": {
        "label": "Same workspace",
        "detail": "Fork conversations or pass context to another agent in the workspace.",
        "source": "https://docs.superset.sh/agent-sessions",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "shared-access",
    "label": "Shared access",
    "category": "handoff",
    "cells": {
      "vibeke": {
        "label": "Scoped invites",
        "detail": "Expiring invitations. View or approval access to a pane or workspace.",
        "source": "/docs/handoff",
        "state": "text"
      },
      "herdr": {
        "label": "Same server",
        "detail": "Multiple clients on the same server. No scoped invitations.",
        "source": "https://herdr.dev/docs/connecting-machines/",
        "state": "text"
      },
      "cmux": {
        "label": "Your devices",
        "detail": "Pair your iPhone with your Mac.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "View / edit",
        "detail": "View or edit access for users and teams.",
        "source": "https://docs.warp.dev/agents/cli/oz-cli/",
        "state": "text"
      },
      "conductor": {
        "label": "Workspace",
        "detail": "Teammates can view a workspace and send agent prompts.",
        "source": "https://www.conductor.build/",
        "state": "text"
      },
      "zellij": {
        "label": "Access tokens",
        "detail": "Full or read-only web access tokens.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "text"
      },
      "tmux": {
        "label": "Same server",
        "detail": "Multiple clients can attach to one session.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "No",
        "detail": "Command definitions can be shared through solo.yml.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "Team setup",
        "detail": "Cloud offering includes managed team environments.",
        "source": "https://emdash.com/cloud",
        "state": "text"
      },
      "superset": {
        "label": "Host members",
        "detail": "Grant or remove organization members at the host level.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "remote-app-previews",
    "label": "Remote app previews",
    "category": "browser",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Remote ports through a local proxy with separate origins.",
        "source": "/docs/previews",
        "state": "yes"
      },
      "herdr": {
        "label": "SSH tunnel",
        "detail": "Use SSH port forwarding.",
        "source": "https://herdr.dev/docs/connecting-machines/",
        "state": "text"
      },
      "cmux": {
        "label": "Yes",
        "detail": "Browser traffic through the remote host.",
        "source": "https://cmux.com/docs/ssh",
        "state": "yes"
      },
      "warp": {
        "label": "Cloud only",
        "detail": "Browser inside the cloud environment.",
        "source": "https://docs.warp.dev/agents/capabilities/computer-use/browser-use/",
        "state": "partial"
      },
      "conductor": {
        "label": "Cloud only",
        "detail": "Cloud workspace previews.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "partial"
      },
      "zellij": {
        "label": "SSH tunnel",
        "detail": "Use SSH port forwarding.",
        "source": "https://zellij.dev/documentation/",
        "state": "text"
      },
      "tmux": {
        "label": "SSH tunnel",
        "detail": "Forward ports with SSH.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "Own tunnel",
        "detail": "Run an external tunnel command.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "Yes",
        "detail": "Preview local apps and remote services.",
        "source": "https://emdash.com/",
        "state": "yes"
      },
      "superset": {
        "label": "Yes",
        "detail": "Forwards workspace ports from the remote host.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "yes"
      }
    },
    "featured": false
  },
  {
    "id": "human-browser-control",
    "label": "Human browser control",
    "category": "browser",
    "cells": {
      "vibeke": {
        "label": "Take over",
        "detail": "View the agent browser. Take control and pause its browser commands.",
        "source": "/docs/previews",
        "state": "text"
      },
      "herdr": {
        "label": "No",
        "detail": "No.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "no"
      },
      "cmux": {
        "label": "Browser pane",
        "detail": "Interact with the browser pane.",
        "source": "https://cmux.com/docs/browser-automation",
        "state": "text"
      },
      "warp": {
        "label": "Session tools",
        "detail": "Use the browser controls in Computer Use environments.",
        "source": "https://docs.warp.dev/agents/capabilities/computer-use/browser-use/",
        "state": "text"
      },
      "conductor": {
        "label": "Annotations",
        "detail": "Browser preview with annotations for the agent.",
        "source": "https://www.conductor.build/changelog/page/5",
        "state": "text"
      },
      "zellij": {
        "label": "No",
        "detail": "Use an external browser.",
        "source": "https://zellij.dev/documentation/",
        "state": "no"
      },
      "tmux": {
        "label": "No",
        "detail": "Use an external browser.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "Use an external browser.",
        "source": "https://soloterm.com/docs/settings/tools-editors-terminals",
        "state": "no"
      },
      "emdash": {
        "label": "Browser pane",
        "detail": "Navigate pages in the task browser.",
        "source": "https://emdash.com/docs/in-app-browser",
        "state": "text"
      },
      "superset": {
        "label": "Design mode",
        "detail": "Browse pages and send selected elements to an agent.",
        "source": "https://docs.superset.sh/browser",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "browser-diagnostics",
    "label": "Browser diagnostics",
    "category": "browser",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Console, network log, DOM snapshots, and screenshots.",
        "source": "/docs/previews",
        "state": "yes"
      },
      "herdr": {
        "label": "External tools",
        "detail": "Use external tools.",
        "source": "https://herdr.dev/docs/plugins/",
        "state": "no"
      },
      "cmux": {
        "label": "Yes",
        "detail": "Console, network activity, DOM snapshots, and screenshots.",
        "source": "https://cmux.com/docs/browser-automation",
        "state": "yes"
      },
      "warp": {
        "label": "Yes",
        "detail": "Playwright output and screenshots.",
        "source": "https://docs.warp.dev/agents/capabilities/computer-use/browser-use/",
        "state": "yes"
      },
      "conductor": {
        "label": "Agent tools",
        "detail": "Browser and agent tools.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "text"
      },
      "zellij": {
        "label": "External tools",
        "detail": "Use external tools.",
        "source": "https://zellij.dev/documentation/",
        "state": "no"
      },
      "tmux": {
        "label": "External tools",
        "detail": "Use browser developer tools.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "External tools",
        "detail": "Use the configured external browser.",
        "source": "https://soloterm.com/docs/settings/tools-editors-terminals",
        "state": "no"
      },
      "emdash": {
        "label": "Dev builds",
        "detail": "Page DevTools are available in development builds.",
        "source": "https://emdash.com/docs/in-app-browser",
        "state": "text"
      },
      "superset": {
        "label": "Yes",
        "detail": "DevTools, console access, screenshots, and CDP.",
        "source": "https://docs.superset.sh/browser",
        "state": "yes"
      }
    },
    "featured": false
  },
  {
    "id": "answer-delivery-after-restart",
    "label": "Answer delivery after restart",
    "category": "agents",
    "cells": {
      "vibeke": {
        "label": "Stored receipts",
        "detail": "Stored request IDs and delivery state prevent duplicate answers.",
        "source": "/docs/agents",
        "state": "text"
      },
      "herdr": {
        "label": "No",
        "detail": "No stored approval-delivery state.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "no"
      },
      "cmux": {
        "label": "Agent-owned",
        "detail": "Agent-dependent.",
        "source": "https://cmux.com/",
        "state": "partial"
      },
      "warp": {
        "label": "Agent-owned",
        "detail": "Agent-dependent.",
        "source": "https://docs.warp.dev/",
        "state": "partial"
      },
      "conductor": {
        "label": "Agent-owned",
        "detail": "Agent-dependent.",
        "source": "https://www.conductor.build/docs",
        "state": "partial"
      },
      "zellij": {
        "label": "Agent-owned",
        "detail": "Agent-dependent.",
        "source": "https://zellij.dev/documentation/",
        "state": "partial"
      },
      "tmux": {
        "label": "Agent-owned",
        "detail": "The agent handles its approval state.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "partial"
      },
      "solo": {
        "label": "Agent-owned",
        "detail": "The agent handles its approval state.",
        "source": "https://soloterm.com/docs/agents",
        "state": "partial"
      },
      "emdash": {
        "label": "Agent-owned",
        "detail": "The agent handles its approval state.",
        "source": "https://emdash.com/",
        "state": "partial"
      },
      "superset": {
        "label": "Agent-owned",
        "detail": "The agent handles its approval state.",
        "source": "https://docs.superset.sh/agent-integration",
        "state": "partial"
      }
    },
    "featured": false
  },
  {
    "id": "phone-access",
    "label": "Phone access",
    "category": "agents",
    "cells": {
      "vibeke": {
        "label": "Web app",
        "detail": "Web app with approvals and push notifications.",
        "source": "/docs/mobile",
        "state": "text"
      },
      "herdr": {
        "label": "SSH app",
        "detail": "Mobile SSH client.",
        "source": "https://herdr.dev/docs/connecting-machines/",
        "state": "text"
      },
      "cmux": {
        "label": "iOS beta",
        "detail": "iOS companion app in beta.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Web app",
        "detail": "Shared sessions through the web app.",
        "source": "https://docs.warp.dev/agents/cli/oz-cli/",
        "state": "text"
      },
      "conductor": {
        "label": "iOS app",
        "detail": "iOS app for cloud workspaces.",
        "source": "https://www.conductor.build/",
        "state": "text"
      },
      "zellij": {
        "label": "Web app",
        "detail": "Mobile web app.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "text"
      },
      "tmux": {
        "label": "SSH app",
        "detail": "Connect through a mobile SSH client.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "No",
        "detail": "Desktop app only.",
        "source": "https://soloterm.com/docs",
        "state": "no"
      },
      "emdash": {
        "label": "SSH + tmux",
        "detail": "Use SSH with a tmux-backed session.",
        "source": "https://emdash.com/docs/tmux-sessions",
        "state": "text"
      },
      "superset": {
        "label": "iOS app",
        "detail": "Connect to the host through the relay.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "access-without-inbound-ports",
    "label": "Access without inbound ports",
    "category": "agents",
    "cells": {
      "vibeke": {
        "label": "Relay",
        "detail": "Encrypted gateway and relay. Pair or revoke each device.",
        "source": "/docs/mobile",
        "state": "text"
      },
      "herdr": {
        "label": "No",
        "detail": "No cloud relay. Remote access uses SSH.",
        "source": "https://herdr.dev/blog/connecting-the-machines/",
        "state": "no"
      },
      "cmux": {
        "label": "Mobile Connect",
        "detail": "Mobile Connect for the iOS app.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Cloud",
        "detail": "Cloud sessions and session sharing.",
        "source": "https://docs.warp.dev/agents/cli/oz-cli/",
        "state": "text"
      },
      "conductor": {
        "label": "Cloud",
        "detail": "Managed cloud workspaces.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "text"
      },
      "zellij": {
        "label": "Own tunnel",
        "detail": "Supply a reverse proxy or tunnel.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "no"
      },
      "tmux": {
        "label": "Own tunnel",
        "detail": "Supply a tunnel for remote access.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "Local API only.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "Own tunnel",
        "detail": "Requires a reachable SSH host or your own tunnel.",
        "source": "https://emdash.com/docs/remote-development/remote-projects",
        "state": "no"
      },
      "superset": {
        "label": "Relay",
        "detail": "The host registers with Superset Relay.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "task-requirements",
    "label": "Task requirements",
    "category": "tasks",
    "cells": {
      "vibeke": {
        "label": "Criteria",
        "detail": "Store task intent, success criteria, and agent run links.",
        "source": "/docs/tasks",
        "state": "text"
      },
      "herdr": {
        "label": "Metadata",
        "detail": "Use workspace metadata or plugins.",
        "source": "https://herdr.dev/docs/plugins/",
        "state": "text"
      },
      "cmux": {
        "label": "Agent prompts",
        "detail": "Use agent prompts and skills.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Plans / rules",
        "detail": "Use prompts, plans, and rules.",
        "source": "https://docs.warp.dev/",
        "state": "text"
      },
      "conductor": {
        "label": "Workspace",
        "detail": "Use chat, attachments, and workspace context.",
        "source": "https://www.conductor.build/docs/concepts/workflow",
        "state": "text"
      },
      "zellij": {
        "label": "External tools",
        "detail": "Use files or plugins.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "no"
      },
      "tmux": {
        "label": "External tools",
        "detail": "Use files or external task tools.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "Todos + notes",
        "detail": "Shared todos, dependencies, and scratchpads.",
        "source": "https://soloterm.com/docs/workflows/agent-orchestration",
        "state": "text"
      },
      "emdash": {
        "label": "Tasks / issues",
        "detail": "Task prompts and imported issue context.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Tasks",
        "detail": "Track work with tasks and agent prompts.",
        "source": "https://docs.superset.sh/tasks",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "diffs-checks-and-review",
    "label": "Diffs, checks, and review",
    "category": "tasks",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Review diffs and evidence. Authorize checks and accept the result.",
        "source": "/docs/tasks",
        "state": "yes"
      },
      "herdr": {
        "label": "Plugins",
        "detail": "Use plugins or external tools.",
        "source": "https://herdr.dev/docs/plugins/",
        "state": "no"
      },
      "cmux": {
        "label": "Agent tools",
        "detail": "Use agent or external review tools.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Yes",
        "detail": "Agent code review and development tools.",
        "source": "https://docs.warp.dev/code/code-editor/",
        "state": "yes"
      },
      "conductor": {
        "label": "Yes",
        "detail": "Diff viewer, checks, comments, and pull request actions.",
        "source": "https://www.conductor.build/docs/reference/diff-viewer",
        "state": "yes"
      },
      "zellij": {
        "label": "Plugins",
        "detail": "Use plugins or external tools.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "no"
      },
      "tmux": {
        "label": "External tools",
        "detail": "Use Git and external review tools.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "Agent tools",
        "detail": "Agents inspect diffs and run project checks.",
        "source": "https://soloterm.com/docs/workflows/agent-orchestration",
        "state": "text"
      },
      "emdash": {
        "label": "Yes",
        "detail": "Diffs, pull requests, and CI checks.",
        "source": "https://emdash.com/",
        "state": "yes"
      },
      "superset": {
        "label": "Yes",
        "detail": "Diff viewer and pull request review.",
        "source": "https://docs.superset.sh/diff-viewer",
        "state": "yes"
      }
    },
    "featured": false
  },
  {
    "id": "review-agents-and-dependencies",
    "label": "Review agents and dependencies",
    "category": "tasks",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Start a review agent. Add blocking or related task links.",
        "source": "/docs/tasks",
        "state": "yes"
      },
      "herdr": {
        "label": "Plugins",
        "detail": "Use agent scripts or plugins.",
        "source": "https://herdr.dev/docs/plugins/",
        "state": "no"
      },
      "cmux": {
        "label": "Agent teams",
        "detail": "Agent teams and orchestration tools.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Orchestration",
        "detail": "Agent orchestration and team workflows.",
        "source": "https://docs.warp.dev/",
        "state": "text"
      },
      "conductor": {
        "label": "Review agents",
        "detail": "Review agents and parallel workspaces.",
        "source": "https://www.conductor.build/docs/concepts/workflow",
        "state": "text"
      },
      "zellij": {
        "label": "Scripts",
        "detail": "Use scripts or plugins.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "no"
      },
      "tmux": {
        "label": "Scripts",
        "detail": "Coordinate agents with shell scripts.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "Yes",
        "detail": "Review agents and todo dependencies.",
        "source": "https://soloterm.com/docs/workflows/agent-orchestration",
        "state": "yes"
      },
      "emdash": {
        "label": "Parallel agents",
        "detail": "Run review work as a separate agent task.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Orchestration",
        "detail": "Coordinate agents across workspaces.",
        "source": "https://docs.superset.sh/orchestration",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "execution-isolation",
    "label": "Execution isolation",
    "category": "runtime",
    "cells": {
      "vibeke": {
        "label": "Providers",
        "detail": "Sandbox or container providers with network policy. Host support varies.",
        "source": "/docs/sandboxes",
        "state": "text"
      },
      "herdr": {
        "label": "No",
        "detail": "Host permissions. Plugins are not sandboxed.",
        "source": "https://herdr.dev/docs/plugins/",
        "state": "no"
      },
      "cmux": {
        "label": "Remote host",
        "detail": "Local host permissions or a separate remote host.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Cloud / host",
        "detail": "Cloud environments or your own execution host.",
        "source": "https://docs.warp.dev/platform/unmanaged-execution/",
        "state": "text"
      },
      "conductor": {
        "label": "Cloud only",
        "detail": "Cloud microVMs. Local worktrees use your permissions.",
        "source": "https://www.conductor.build/docs/cloud/faq",
        "state": "partial"
      },
      "zellij": {
        "label": "Plugins only",
        "detail": "WASM plugin sandbox. Terminal commands use host permissions.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "text"
      },
      "tmux": {
        "label": "No",
        "detail": "Commands use the host user permissions.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "no"
      },
      "solo": {
        "label": "No",
        "detail": "Manages local processes. Containers are external.",
        "source": "https://soloterm.com/",
        "state": "no"
      },
      "emdash": {
        "label": "Own provider",
        "detail": "Worktrees separate files. Supply isolated hosts through workspace providers.",
        "source": "https://emdash.com/",
        "state": "partial"
      },
      "superset": {
        "label": "Own host",
        "detail": "Worktrees separate files. Supply an isolated host when required.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "partial"
      }
    },
    "featured": false
  },
  {
    "id": "commands-and-api",
    "label": "Commands and API",
    "category": "runtime",
    "cells": {
      "vibeke": {
        "label": "CLI + RPC",
        "detail": "CLI and JSON-RPC for agents, tasks, requests, and browsers.",
        "source": "/docs/api",
        "state": "text"
      },
      "herdr": {
        "label": "CLI + socket",
        "detail": "CLI, socket API, and events.",
        "source": "https://herdr.dev/docs/socket-api/",
        "state": "text"
      },
      "cmux": {
        "label": "CLI + socket",
        "detail": "CLI and socket API.",
        "source": "https://cmux.com/docs/browser-automation",
        "state": "text"
      },
      "warp": {
        "label": "CLI + API",
        "detail": "CLI, API, SDK, and MCP.",
        "source": "https://docs.warp.dev/agents/cli/oz-cli/",
        "state": "text"
      },
      "conductor": {
        "label": "API",
        "detail": "Conductor API.",
        "source": "https://www.conductor.build/docs/api",
        "state": "text"
      },
      "zellij": {
        "label": "CLI + plugins",
        "detail": "CLI, actions, and plugin API.",
        "source": "https://zellij.dev/documentation/commands.html",
        "state": "text"
      },
      "tmux": {
        "label": "CLI / control",
        "detail": "Shell commands and control mode.",
        "source": "https://github.com/tmux/tmux/wiki/Control-Mode",
        "state": "text"
      },
      "solo": {
        "label": "CLI / HTTP / MCP",
        "detail": "Control the running app with CLI, HTTP, or MCP.",
        "source": "https://soloterm.com/docs",
        "state": "text"
      },
      "emdash": {
        "label": "Agent tools",
        "detail": "Configure provider CLIs and MCP tools.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "CLI / SDK / MCP",
        "detail": "Control hosts, workspaces, and agents.",
        "source": "https://docs.superset.sh/cli/host-server",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "extensions",
    "label": "Extensions",
    "category": "runtime",
    "cells": {
      "vibeke": {
        "label": "Actions / hooks",
        "detail": "Actions, hooks, and plugin panes. Partial Herdr plugin support.",
        "source": "/docs/migrating-from-herdr",
        "state": "text"
      },
      "herdr": {
        "label": "Plugins",
        "detail": "Plugins, GitHub installation, and marketplace.",
        "source": "https://herdr.dev/docs/plugins/",
        "state": "no"
      },
      "cmux": {
        "label": "Skills / hooks",
        "detail": "Agent skills and hooks.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Skills / MCP",
        "detail": "Skills and MCP integrations.",
        "source": "https://docs.warp.dev/",
        "state": "text"
      },
      "conductor": {
        "label": "Agent tools",
        "detail": "Agent tools and repository scripts.",
        "source": "https://www.conductor.build/docs/concepts/workflow",
        "state": "text"
      },
      "zellij": {
        "label": "WASM plugins",
        "detail": "WASM plugins.",
        "source": "https://zellij.dev/documentation/plugins.html",
        "state": "text"
      },
      "tmux": {
        "label": "Scripts / hooks",
        "detail": "Extend tmux with scripts and hooks.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "text"
      },
      "solo": {
        "label": "MCP / templates",
        "detail": "Custom agents, prompt templates, and MCP tools.",
        "source": "https://soloterm.com/docs",
        "state": "text"
      },
      "emdash": {
        "label": "Skills / MCP",
        "detail": "Library for prompts, skills, and MCP servers.",
        "source": "https://emdash.com/",
        "state": "text"
      },
      "superset": {
        "label": "Skills / MCP",
        "detail": "Agent skills and an MCP server.",
        "source": "https://docs.superset.sh/agent-integration",
        "state": "text"
      }
    },
    "featured": false
  },
  {
    "id": "existing-terminal",
    "label": "Use your existing terminal",
    "category": "overview",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Runs inside your current terminal.",
        "source": "/docs/introduction",
        "state": "yes"
      },
      "herdr": {
        "label": "Yes",
        "detail": "Runs inside your current terminal.",
        "source": "https://herdr.dev/docs/",
        "state": "yes"
      },
      "cmux": {
        "label": "No",
        "detail": "Uses its own app interface. A CLI can control the app where supported.",
        "source": "https://cmux.com/",
        "state": "no"
      },
      "warp": {
        "label": "No",
        "detail": "Uses its own app interface. A CLI can control the app where supported.",
        "source": "https://docs.warp.dev/",
        "state": "no"
      },
      "conductor": {
        "label": "No",
        "detail": "Uses its own app interface. A CLI can control the app where supported.",
        "source": "https://www.conductor.build/docs",
        "state": "no"
      },
      "zellij": {
        "label": "Yes",
        "detail": "Runs inside your current terminal.",
        "source": "https://zellij.dev/documentation/",
        "state": "yes"
      },
      "tmux": {
        "label": "Yes",
        "detail": "Runs inside your current terminal.",
        "source": "https://github.com/tmux/tmux/wiki",
        "state": "yes"
      },
      "solo": {
        "label": "No",
        "detail": "Uses its own app interface. A CLI can control the app where supported.",
        "source": "https://soloterm.com/docs",
        "state": "no"
      },
      "emdash": {
        "label": "No",
        "detail": "Uses its own app interface. A CLI can control the app where supported.",
        "source": "https://emdash.com/docs",
        "state": "no"
      },
      "superset": {
        "label": "No",
        "detail": "Uses its own app interface. A CLI can control the app where supported.",
        "source": "https://docs.superset.sh/overview",
        "state": "no"
      }
    },
    "featured": false
  },
  {
    "id": "multiple-clients",
    "label": "Multiple clients per runtime",
    "category": "overview",
    "cells": {
      "vibeke": {
        "label": "Yes",
        "detail": "Terminal, browser, and desktop clients connect to the runtime.",
        "source": "/docs/mobile",
        "state": "yes"
      },
      "herdr": {
        "label": "Yes",
        "detail": "Multiple clients attach to the same server.",
        "source": "https://herdr.dev/docs/connecting-machines/",
        "state": "yes"
      },
      "cmux": {
        "label": "Mobile Connect",
        "detail": "The iOS companion connects to the Mac.",
        "source": "https://cmux.com/",
        "state": "text"
      },
      "warp": {
        "label": "Cloud runs",
        "detail": "Desktop, CLI, web, and API access the same cloud run.",
        "source": "https://docs.warp.dev/platform/quickstart/",
        "state": "text"
      },
      "conductor": {
        "label": "Cloud runs",
        "detail": "Teammates connect to a shared cloud workspace.",
        "source": "https://www.conductor.build/",
        "state": "text"
      },
      "zellij": {
        "label": "Yes",
        "detail": "Terminal and web clients attach to the session.",
        "source": "https://zellij.dev/documentation/web-client.html",
        "state": "yes"
      },
      "tmux": {
        "label": "Yes",
        "detail": "Several terminal clients can attach to one session.",
        "source": "https://github.com/tmux/tmux/wiki/Getting-Started",
        "state": "yes"
      },
      "solo": {
        "label": "App + API",
        "detail": "CLI and API control the running desktop app.",
        "source": "https://soloterm.com/docs",
        "state": "text"
      },
      "emdash": {
        "label": "With tmux",
        "detail": "Attach to the underlying tmux session.",
        "source": "https://emdash.com/docs/tmux-sessions",
        "state": "partial"
      },
      "superset": {
        "label": "Yes",
        "detail": "Desktop, iPhone, and CLI access the same host service.",
        "source": "https://docs.superset.sh/remote-access",
        "state": "yes"
      }
    },
    "featured": false
  }
]

export const comparisonGroups = [
  { id: 'runtime', label: 'Runtime' },
  { id: 'agents', label: 'Agent control' },
  { id: 'handoff', label: 'Machines and teams' },
  { id: 'browser', label: 'Browser' },
  { id: 'tasks', label: 'Tasks and review' },
  { id: 'overview', label: 'Interface and tools' },
]
export const comparisonCount = comparisonRows.length

export function comparisonSections(category: string) {
  if (category !== 'all') {
    const group = comparisonGroups.find(group => group.id === category)
    return group ? [{ ...group, rows: comparisonRows.filter(row => row.category === category) }] : []
  }
  return [
    { id: 'key', label: 'Key features', rows: comparisonRows.filter(row => row.featured) },
    ...comparisonGroups.map(group => ({ ...group, rows: comparisonRows.filter(row => row.category === group.id && !row.featured) })),
  ].filter(group => group.rows.length)
}
