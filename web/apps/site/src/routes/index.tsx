import { createFileRoute, Link } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight, BookOpen, ChevronRight, Code2, GitBranch, Globe2, Layers, Radio, ShieldCheck, Terminal } from 'lucide-react'
import { useState } from 'react'
import { CodeBlock, Eyebrow } from '../components/ui'
import { ClientShowcase } from '../components/client-showcase'
import { comparisonCount } from '../lib/comparisons'

export const Route = createFileRoute('/')({ component: Home })

const capabilities = [
  { icon: Radio, number: '01', title: 'Answer agent requests.', body: 'Read permission requests and questions in the inbox. Send your answer through the agent integration.', link: 'How interactions work', slug: 'agents', tag: 'Claude hooks · Codex RPC · pi extension' },
  { icon: Layers, number: '02', title: 'Restart the server.', body: 'A holder process owns each terminal. If the server stops, the holder keeps the agent process active. The new server reconnects.', link: 'What survives a restart', slug: 'holders', tag: 'one holder process per pane' },
  { icon: GitBranch, number: '03', title: 'Use separate worktrees.', body: 'Use a separate branch for each task. Review the task diff and check results before you accept the work.', link: 'Tasks and review', slug: 'tasks', tag: 'separate branches · reviewable diffs' },
]
const commands = {
  Local: { title: 'Start a workspace.', text: 'Run Vibeke. Create a workspace for your repository. Start an agent. Close the terminal to disconnect. Run Vibeke again to reconnect.', code: '# Attach the terminal workspace\nvibeke\n\n# From another shell, in your project\nvibeke workspace create .\nvibeke agent start builder --harness claude', slug: 'quickstart' },
  Remote: { title: 'Connect to a remote host.', text: 'Use SSH to connect to your agent host. Vibeke forwards remote app previews to your local browser.', code: '# Attach to your development machine\nvibeke ssh devbox\n\n# Inspect connected machines\nvibeke machine list\nvibeke machine status devbox', slug: 'previews' },
  Script: { title: 'Use the control API.', text: 'Use a script to read agent output or wait for a state change. The CLI and terminal interface use the same API.', code: '# Inspect the current session\nvibeke agent list --json\nvibeke interaction list --status open\n\n# Discover the available API\nvibeke api methods\nvibeke --skill', slug: 'cli' },
} as const

function Home() {
  const [mode, setMode] = useState<keyof typeof commands>('Local')
  const command = commands[mode]
  return <main id="main">
    <section className="grid-surface border-b border-line">
      <div className="page-width pt-14 pb-12 sm:pt-20 sm:pb-14">
        <div className="flex items-center gap-3"><span className="h-1.5 w-1.5 bg-accent" /><Eyebrow>A terminal workspace for coding agents</Eyebrow><span className="ml-auto hidden font-mono text-[10px] text-muted sm:inline">LOCAL + SSH</span></div>
        <div className="mt-7 grid items-end gap-8 lg:grid-cols-[1.65fr_1fr] lg:gap-16">
          <h1 className="text-[clamp(3.1rem,6.6vw,5.75rem)] leading-[1.02] font-medium tracking-[-0.065em]">Run your agents.<br /><span className="text-accent">Keep control.</span></h1>
          <div className="pb-1"><p className="max-w-md text-[15px] leading-7 text-muted">Run Claude Code, Codex, and other agent CLIs in one workspace. Reconnect to the same processes after you disconnect. Answer agent requests from the inbox.</p><div className="mt-7 flex flex-wrap gap-3"><Link to="/docs/$slug" params={{ slug: 'install' }} className="button-primary">Install Vibeke <ArrowUpRight size={16} /></Link><Link to="/docs" className="button-secondary">Read the docs <ArrowRight size={14} /></Link></div><p className="mt-4 font-mono text-[10px] text-muted">macOS + Linux <span className="mx-2">/</span> Rust <span className="mx-2">/</span> Apache-2.0</p></div>
        </div>
        <div className="mt-12 sm:mt-14"><ClientShowcase /></div>
        <div className="mt-9 flex flex-wrap items-center justify-between gap-x-8 gap-y-5"><Eyebrow>Agent integrations</Eyebrow><div className="flex flex-wrap items-center gap-x-8 gap-y-4 font-mono text-xs text-[#c8ccc2] sm:text-sm"><span className="flex items-center gap-2"><span className="text-accent">✳</span> Claude Code</span><span className="flex items-center gap-2"><Terminal size={16} /> Codex</span><span className="flex items-center gap-2"><span className="text-green">π</span> pi / omp</span><Link to="/docs/$slug" params={{ slug: 'agents' }} className="text-[11px] text-muted hover:text-cream">Integration details <ArrowUpRight className="inline" size={12} /></Link></div></div>
      </div>
    </section>

    <section className="page-width py-20 sm:py-24" id="workspace">
      <div className="grid gap-6 md:grid-cols-2"><div><Eyebrow><span className="mr-3 text-accent">01 /</span> While the agents run</Eyebrow><h2 className="section-title mt-5">Find the agent<br /><span className="text-muted">that needs an answer.</span></h2></div><p className="max-w-md self-end text-sm leading-7 text-muted md:ml-auto">Claude edits a test. Codex reviews a diff. When an agent needs an answer, the inbox shows its request and task.</p></div>
      <div className="mt-12 grid border-t border-line md:grid-cols-3">{capabilities.map(({ icon: Icon, ...feature }, i) => <article key={feature.number} className={`border-b border-line py-8 md:border-b-0 ${i ? 'md:border-l md:pl-8' : ''} ${i < 2 ? 'md:pr-8' : ''}`}><div className="mb-8 flex items-center justify-between"><Icon size={23} strokeWidth={1.4} className="text-accent" /><span className="font-mono text-[10px] text-muted">[{feature.number}]</span></div><h3 className="text-lg font-medium tracking-tight">{feature.title}</h3><p className="mt-3 text-sm leading-7 text-muted">{feature.body}</p><div className="my-6 border-l border-line pl-3 font-mono text-[10px] text-green">{feature.tag}</div><Link to="/docs/$slug" params={{ slug: feature.slug }} className="flex items-center gap-2 text-xs text-cream hover:text-accent">{feature.link} <ArrowRight size={13} /></Link></article>)}</div>
    </section>

    <section className="section bg-panel/40"><div className="page-width grid grid-cols-1 gap-12 lg:grid-cols-2 lg:gap-20"><div><Eyebrow><span className="mr-3 text-accent">02 /</span> From your shell</Eyebrow><h2 className="section-title mt-5">Start locally.<br />Attach over SSH.</h2><div className="mt-8 flex gap-1" aria-label="Command examples">{(Object.keys(commands) as (keyof typeof commands)[]).map(key => <button key={key} type="button" aria-pressed={mode === key} onClick={() => setMode(key)} className={`border px-4 py-2 font-mono text-[11px] ${mode === key ? 'border-accent/40 bg-accent/10 text-accent' : 'border-transparent text-muted hover:text-cream'}`}>{key === 'Local' ? <Terminal className="mr-2 inline" size={12} /> : key === 'Remote' ? <Globe2 className="mr-2 inline" size={12} /> : <Code2 className="mr-2 inline" size={12} />}{key}</button>)}</div><div className="mt-6" aria-live="polite"><h3 className="text-base">{command.title}</h3><p className="mt-3 max-w-md text-sm leading-7 text-muted">{command.text}</p></div><Link className="text-link mt-6 text-xs" to="/docs/$slug" params={{ slug: command.slug }}>Follow the guide <ArrowRight size={14} /></Link></div><div className="self-center"><CodeBlock code={command.code} title={`~/workspace — ${mode.toLowerCase()}`} /><div className="mt-4 flex items-start gap-2 font-mono text-[10px] leading-5 text-muted"><ShieldCheck size={13} className="mt-0.5 shrink-0 text-green" /><span>Host mode uses your permissions. Use a supported sandbox, container, or VM to limit access.</span></div></div></div></section>

    <section className="section">
      <div className="page-width">
        <div className="flex flex-wrap items-end justify-between gap-6">
          <div><Eyebrow><span className="mr-3 text-accent">03 /</span> Comparison</Eyebrow><h2 className="section-title mt-5">How it compares.</h2></div>
          <Link to="/compare" className="text-link text-xs">Compare the features <ArrowUpRight size={15} /></Link>
        </div>
        <p className="mt-5 max-w-xl text-sm leading-7 text-muted">Compare Vibeke with other terminal tools and agent apps. The table covers agent control, remote access, browser tools, and review.</p>
        <div className="mt-8 grid gap-6 border-y border-line py-8 md:grid-cols-3">{[
          ['Transfer work between hosts.', 'Copy code changes and an available agent conversation to another paired host.', 'handoff'],
          ['Share work with a teammate.', 'Transfer work to their host, or give them view or approval access.', 'handoff'],
          ['Control the browser.', 'Open a browser pane. View the agent session and take control when necessary.', 'previews'],
        ].map(([title, body, slug]) => <Link key={title} to="/docs/$slug" params={{ slug: slug! }} className="group"><h3 className="text-sm group-hover:text-accent">{title}</h3><p className="mt-2 text-xs leading-6 text-muted">{body}</p></Link>)}</div>
        <Link to="/compare" className="text-link mt-6 text-xs">Compare {comparisonCount} features <ArrowRight size={14} /></Link>
      </div>
    </section>

    <section className="section bg-panel/40"><div className="page-width grid gap-10 md:grid-cols-[1fr_1.15fr] md:gap-20"><div><Eyebrow><span className="mr-3 text-accent">04 /</span> Documentation</Eyebrow><h2 className="section-title mt-5">Start with<br />one workspace.</h2><p className="mt-5 max-w-sm text-sm leading-7 text-muted">Use the quickstart to start your first agent. The reference lists commands, API methods, and configuration settings from the code.</p><Link to="/docs" className="text-link mt-7 text-xs">Open the docs <ArrowRight size={14} /></Link></div><div className="border-t border-line">{[{ icon: Terminal, title: 'Start your first workspace', description: 'Install Vibeke. Start an agent. Reconnect to the session.', slug: 'quickstart' }, { icon: BookOpen, title: 'How the runtime works', description: 'Agent processes, requests, and tasks.', slug: 'introduction' }, { icon: Code2, title: 'CLI reference', description: 'Commands and configuration, from the code.', slug: 'cli' }].map(({ icon: Icon, ...item }) => <Link key={item.slug} to="/docs/$slug" params={{ slug: item.slug }} className="group flex items-center gap-5 border-b border-line py-6"><Icon size={20} strokeWidth={1.5} className="shrink-0 text-muted group-hover:text-accent" /><div><h3 className="text-sm group-hover:text-accent">{item.title}</h3><p className="mt-2 text-xs text-muted">{item.description}</p></div><ChevronRight size={16} className="ml-auto shrink-0 text-muted group-hover:translate-x-1" /></Link>)}</div></div></section>

    <section className="page-width py-20 sm:py-24"><div className="grid gap-10 md:grid-cols-[1fr_1.5fr] md:gap-20"><div><Eyebrow>Before you install</Eyebrow><h2 className="mt-5 text-3xl font-medium tracking-tight">Common questions.</h2></div><div>{[
      ['Does Vibeke replace my agent?', 'No. Vibeke provides the workspace. Use your existing agents and model accounts. Claude Code, Codex, and pi/omp support structured integrations. Features depend on the agent.'],
      ['What survives a restart?', 'Holder processes keep agents active after a server restart or client disconnect. Screen recovery can be incomplete. A host restart or holder failure stops the processes.'],
      ['Does every task run in a sandbox?', 'No. Host mode uses your permissions. Use a supported sandbox, container, or VM to limit access. Worktrees separate file changes. They do not prevent access.'],
      ['Can I try it alongside Herdr?', 'Yes. Import supported configuration and layouts into Vibeke. Herdr processes remain in Herdr. Use the dry-run command in the migration guide first.'],
    ].map(([question, answer]) => <details key={question} className="group border-b border-line first:border-t"><summary className="flex min-h-16 list-none items-center justify-between gap-4 py-5 text-sm [&::-webkit-details-marker]:hidden">{question}<span className="font-mono text-lg text-muted group-open:rotate-45">+</span></summary><p className="max-w-xl pb-6 pr-8 text-sm leading-7 text-muted">{answer}</p></details>)}</div></div></section>

    <section className="border-t border-line bg-accent/[0.025]"><div className="page-width flex flex-col items-start justify-between gap-8 py-14 sm:flex-row sm:items-center"><div className="flex items-center gap-5"><img src="/brand/duck-128.png" width={80} height={80} alt="" className="hidden size-20 shrink-0 sm:block" /><div><h2 className="text-2xl font-medium tracking-tight sm:text-3xl">Try it in one project.</h2><p className="mt-2 text-sm text-muted">Start an agent. Disconnect. Reconnect to the same session.</p></div></div><Link className="button-primary shrink-0" to="/docs/$slug" params={{ slug: 'install' }}>Install Vibeke <ArrowRight size={15} /></Link></div></section>
  </main>
}
