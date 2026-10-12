import { createFileRoute, Link } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight, BookOpen, ChevronRight, Code2, GitBranch, Globe2, Layers, Radio, ShieldCheck, Terminal, Star, Smartphone } from 'lucide-react'
import { useState } from 'react'
import { CodeBlock, Eyebrow } from '../components/ui'
import { ClientShowcase } from '../components/client-showcase'
import { ProductTour } from '../components/product-tour'
import { DemoVideo } from '../components/demo-video'
import { trackSiteEvent } from '../lib/analytics'
import { comparisonCount } from '../lib/comparisons'
import { latestRelease } from '../lib/release'
import { pageHead, siteTitle, siteDescription } from '../lib/seo'

export const Route = createFileRoute('/')({
  head: () => pageHead({ path: '/', title: siteTitle, description: siteDescription }),
  component: Home,
})

const capabilities = [
  { icon: Radio, number: '01', title: 'Know which agent needs you.', body: 'Keep agent questions and permission requests in one inbox. Answer through the supported agent integration.', link: 'Agents and interactions', slug: 'agents', tag: 'Claude Code · Codex · pi / omp' },
  { icon: Layers, number: '02', title: 'Reconnect to work in progress.', body: 'Agents keep running after a client disconnect or server restart. A host restart stops the processes.', link: 'What survives a restart', slug: 'holders', tag: 'your work stays on the host' },
  { icon: GitBranch, number: '03', title: 'Give each task its own branch.', body: 'Use a separate worktree for each task. Review its changes and check results before you accept the work.', link: 'Tasks and review', slug: 'tasks', tag: 'separate files · reviewable changes' },
]
const stories = [
  { id: 'phone', icon: Smartphone, title: 'Answer from your phone.', body: 'Pair your phone with a running host. Read an agent request, then allow or deny it. Your agent keeps working on the host.', detail: 'Requests depend on the agent integration. The host must stay awake and connected.', slug: 'mobile', at: 115.5, action: 'Watch phone approvals' },
  { id: 'remote-preview', icon: Globe2, title: 'Preview work on your devbox.', body: 'Run an app on an SSH host. Open its preview beside your agent in your local terminal.', detail: 'Browser panes need Chromium and a terminal that supports graphics.', slug: 'previews', at: 32, action: 'Watch a remote preview' },
] as const
const questions = [
  ['Which computers can run Vibeke?', 'The host CLI runs on macOS 13 or later and Linux. macOS supports Apple silicon and Intel. Linux supports x86_64 and ARM64. The Windows desktop app connects to a supported remote host.'],
  ['Can I use my existing agents and accounts?', 'Yes. Run your usual agent commands in a pane. Agent and model charges remain with your providers. Structured requests and other features depend on the integration.'],
  ['How do the desktop and browser apps connect?', 'Both connect to a running Vibeke host. The desktop app does not bundle the CLI. Install the CLI for local use, or pair with a remote host. Pair your browser or phone from the host.'],
  ['What survives a restart?', 'Holder processes keep agents active after a server restart or client disconnect. Screen recovery can be incomplete. A host restart or holder failure stops the processes.'],
  ['Does every task run in a sandbox?', 'No. Host mode uses your permissions. Use a supported sandbox, container, or VM to limit access. Worktrees separate file changes. They do not prevent access.'],
] as const
const commands = {
  Local: { title: 'Start a workspace.', text: 'Start Vibeke in your project. Complete setup, then run your usual agent command in a pane. Use the command palette for workspaces, splits, and the inbox.', code: '# In your project directory\ncd your-project\nvibeke\n\n# Inside a Vibeke pane\nclaude  # or codex, pi, omp', slug: 'quickstart' },
  Remote: { title: 'Connect to a remote host.', text: 'Configure your SSH host to download a signed release, then connect. Your agents and files stay on the host.', code: '# ~/.config/vibeke/config.toml\n[[remote.machine]]\nlabel = "devbox"\naddress = "devbox"\nbootstrap = "remote-download"\n\n# Then connect from your terminal\nvibeke ssh devbox', slug: 'remote' },
  Script: { title: 'Use the control API.', text: 'Use a script to read agent output or wait for a state change. The CLI and terminal interface use the same API.', code: '# Inspect the current session\nvibeke agent list --json\nvibeke interaction list --status open\n\n# Discover the available API\nvibeke api methods\nvibeke --skill', slug: 'cli' },
} as const

function Home() {
  const [mode, setMode] = useState<keyof typeof commands>('Local')
  const command = commands[mode]
  return <main id="main">
    <section className="grid-surface border-b border-line">
      <div className="page-width py-8 sm:py-14">
        <Eyebrow>A terminal workspace for coding agents</Eyebrow>
        <div className="mt-5 grid items-center gap-7 lg:grid-cols-[0.9fr_1.1fr] lg:gap-12">
          <div>
            <h1 className="text-[clamp(2.15rem,4.3vw,3.8rem)] leading-[1.06] font-medium tracking-[-0.055em]">Run agents in your terminal.<br /><span className="text-accent">Answer from your phone.</span></h1>
            <p className="mt-5 max-w-lg text-sm leading-6 text-muted sm:text-[16px] sm:leading-7">Bring Claude Code, Codex, and your other agent CLIs into one workspace. Run them on your laptop or an SSH host. Pair your phone to answer supported requests.</p>
            <div className="mt-6 flex flex-wrap gap-3">
              <a href="#install" className="button-primary" onClick={() => trackSiteEvent('install_intent', { placement: 'hero', target: 'first-use' })}>Install Vibeke <ArrowRight size={15} /></a>
              <a href="https://github.com/vibeke-ai/vibeke" className="button-secondary" onClick={() => trackSiteEvent('github_outbound', { placement: 'hero', target: 'repository' })}><Star size={15} /> Star on GitHub</a>
            </div>
            <p className="mt-4 text-xs text-muted">Open source · Apache-2.0 · <a href={latestRelease.url} className="underline underline-offset-4 hover:text-cream">v{latestRelease.version}</a></p>
          </div>
          <DemoVideo />
        </div>
      </div>
    </section>

    <section id="install" className="section">
      <div className="page-width grid grid-cols-1 gap-10 lg:grid-cols-2 lg:gap-16">
        <div>
          <Eyebrow>Start with one project</Eyebrow>
          <h2 className="section-title mt-4">Keep your usual<br />agent commands.</h2>
          <ol className="mt-6 space-y-4 text-sm leading-7 text-muted">
            <li><strong className="text-cream">1. Install the host CLI.</strong> Use macOS 13+ (Apple silicon or Intel) or Linux (x86_64 or ARM64).</li>
            <li><strong className="text-cream">2. Open your project.</strong> Run <code className="text-cream">vibeke</code> in its directory. Complete first-run setup.</li>
            <li><strong className="text-cream">3. Start an agent.</strong> Run <code className="text-cream">claude</code>, <code className="text-cream">codex</code>, or your usual command inside the pane.</li>
          </ol>
          <Link to="/docs/$slug" params={{ slug: 'quickstart' }} className="text-link mt-6">Follow the quickstart <ArrowRight size={14} /></Link>
        </div>
        <div>
          <p className="mb-4 text-sm leading-6 text-muted">Install <code className="text-cream">minisign</code> first: <code className="text-cream">brew install minisign</code> on macOS, or use your Linux package manager.</p>
          <CodeBlock title="install the CLI · requires minisign" code="curl -fsSL https://vibeke.dev/install.sh | sh" onCopied={() => trackSiteEvent('install_copy', { placement: 'first-use', target: 'cli' })} />
          <div className="mt-4"><CodeBlock title="in your project directory" code={'cd your-project\nvibeke'} /></div>
          <Link to="/docs/$slug" params={{ slug: 'install' }} className="text-link mt-4 text-xs" onClick={() => trackSiteEvent('install_intent', { placement: 'first-use', target: 'install-guide' })}>Installation requirements and verification <ArrowRight size={13} /></Link>
        </div>
      </div>
    </section>

    <section className="page-width py-20 sm:py-24" id="workspace">
      <div className="grid gap-6 md:grid-cols-2"><div><Eyebrow><span className="mr-3 text-accent">01 /</span> While the agents run</Eyebrow><h2 className="section-title mt-5">Find the agent<br /><span className="text-muted">that needs an answer.</span></h2></div><p className="max-w-md self-end text-sm leading-7 text-muted md:ml-auto">Claude edits a test. Codex reviews a diff. When an agent needs an answer, the inbox shows its request and task.</p></div>
      <div className="mt-12 grid border-t border-line md:grid-cols-3">{capabilities.map(({ icon: Icon, ...feature }, i) => <article key={feature.number} className={`border-b border-line py-8 md:border-b-0 ${i ? 'md:border-l md:pl-8' : ''} ${i < 2 ? 'md:pr-8' : ''}`}><div className="mb-8 flex items-center justify-between"><Icon size={23} strokeWidth={1.4} className="text-accent" /><span className="font-mono text-[10px] text-muted">[{feature.number}]</span></div><h3 className="text-lg font-medium tracking-tight">{feature.title}</h3><p className="mt-3 text-sm leading-7 text-muted">{feature.body}</p><div className="my-6 border-l border-line pl-3 font-mono text-[10px] text-green">{feature.tag}</div><Link to="/docs/$slug" params={{ slug: feature.slug }} className="flex items-center gap-2 text-xs text-cream hover:text-accent">{feature.link} <ArrowRight size={13} /></Link></article>)}</div>
    </section>

    <section className="section bg-panel/40" id="workflows">
      <div className="page-width">
        <Eyebrow>See a useful workflow</Eyebrow>
        <h2 className="section-title mt-4">From the terminal<br />to the next step.</h2>
        <div className="mt-10 grid gap-8 md:grid-cols-2">
          {stories.map(({ id, icon: Icon, title, body, detail, slug, at, action }) => <article id={id} key={id} className="border border-line bg-base p-6 sm:p-8">
            <Icon className="text-accent" size={24} /><h3 className="mt-5 text-xl font-medium">{title}</h3>
            <p className="mt-3 text-sm leading-7 text-muted">{body}</p>
            <a href={`?t=${at}#tour`} className="text-link mt-5 text-sm">{action} <ArrowRight size={14} /></a>
            <p className="mt-4 text-xs leading-6 text-muted">{detail} <Link to="/docs/$slug" params={{ slug }} className="text-accent underline underline-offset-4">Read the guide</Link></p>
          </article>)}
          <article id="agent-review" className="border border-line bg-base p-6 sm:p-8">
            <Eyebrow>Example · agent mentions</Eyebrow><h3 className="mt-5 text-xl font-medium">Ask another agent for a review.</h3>
            <p className="mt-3 text-sm leading-7 text-muted">Name a pane <code>@reviewer</code>. Send it a request from another agent pane in the same workspace.</p>
            <pre className="mt-5 whitespace-pre-wrap border-l border-accent pl-4 text-sm leading-6">@reviewer please check the changes in src/auth.rs</pre>
            <p className="mt-4 text-xs leading-6 text-muted">Mentions work with Claude Code and Codex integrations. Messages wait until the receiving agent is idle.</p>
            <Link to="/docs/$slug" params={{ slug: 'mentions' }} className="text-link mt-5 text-sm">Set up agent mentions <ArrowRight size={14} /></Link>
          </article>
          <article id="page-feedback" className="border border-line bg-base p-6 sm:p-8">
            <Eyebrow>Example · page annotations</Eyebrow><h3 className="mt-5 text-xl font-medium">Show your agent what to change.</h3>
            <ol className="mt-4 list-decimal space-y-3 pl-5 text-sm leading-7 text-muted"><li>Open your app in a browser pane.</li><li>Choose <strong className="text-cream">Annotate page</strong> in the command palette.</li><li>Pick an element. Write a note such as “Make this button easier to read.”</li><li>Send the notes to your agent.</li></ol>
            <p className="mt-4 text-xs leading-6 text-muted">The notes include the selected element and a screenshot. They wait while the agent is busy.</p>
            <Link to="/docs/$slug" params={{ slug: 'previews' }} hash="annotate-a-page-for-an-agent" className="text-link mt-5 text-sm">Use page annotations <ArrowRight size={14} /></Link>
          </article>
        </div>
      </div>
    </section>

    <section className="section" id="connect">
      <div className="page-width">
        <Eyebrow>Ways to connect</Eyebrow><h2 className="section-title mt-4">One host.<br />Your choice of screen.</h2>
        <p className="mt-5 max-w-xl text-sm leading-7 text-muted">Start in your terminal. Add the desktop app or pair your browser with the running host when you want another view.</p>
        <div className="mt-10"><ClientShowcase /></div>
        <div className="mt-8 grid gap-6 border-t border-line pt-8 md:grid-cols-2">
          <div><h3 className="text-lg">Desktop app</h3><p className="mt-2 text-sm leading-7 text-muted">Available for macOS, Linux, and Windows. The desktop app connects to a host. Local use also requires the CLI. Windows connects to a supported remote host.</p><Link to="/docs/$slug" params={{ slug: 'desktop' }} className="text-link mt-4" onClick={() => trackSiteEvent('connection_intent', { placement: 'connect', target: 'desktop-guide' })}>Download desktop <ArrowRight size={14} /></Link></div>
          <div><h3 className="text-lg">Browser and phone</h3><p className="mt-2 text-sm leading-7 text-muted">Open Devices on the host, then choose Pair a phone. Confirm the pairing to connect. Your host must stay awake and online.</p><div className="mt-4 flex flex-wrap gap-5"><Link to="/docs/$slug" params={{ slug: 'mobile' }} className="text-link">Pair your phone <ArrowRight size={14} /></Link><a href="https://app.vibeke.dev" className="text-link" onClick={() => trackSiteEvent('connection_intent', { placement: 'connect', target: 'paired-browser' })}>Open browser app <ArrowUpRight size={14} /></a></div></div>
        </div>
      </div>
    </section>

    <ProductTour />

    <section className="section bg-panel/40"><div className="page-width grid grid-cols-1 gap-12 lg:grid-cols-2 lg:gap-20"><div><Eyebrow><span className="mr-3 text-accent">02 /</span> From your shell</Eyebrow><h2 className="section-title mt-5">Start locally.<br />Attach over SSH.</h2><div className="mt-8 flex gap-1" aria-label="Command examples">{(Object.keys(commands) as (keyof typeof commands)[]).map(key => <button key={key} type="button" aria-pressed={mode === key} onClick={() => setMode(key)} className={`border px-4 py-2 font-mono text-[11px] ${mode === key ? 'border-accent/40 bg-accent/10 text-accent' : 'border-transparent text-muted hover:text-cream'}`}>{key === 'Local' ? <Terminal className="mr-2 inline" size={12} /> : key === 'Remote' ? <Globe2 className="mr-2 inline" size={12} /> : <Code2 className="mr-2 inline" size={12} />}{key}</button>)}</div><div className="mt-6" aria-live="polite"><h3 className="text-[16px]">{command.title}</h3><p className="mt-3 max-w-md text-sm leading-7 text-muted">{command.text}</p></div><Link className="text-link mt-6 text-xs" to="/docs/$slug" params={{ slug: command.slug }}>Follow the guide <ArrowRight size={14} /></Link></div><div className="self-center"><CodeBlock code={command.code} title={`~/workspace — ${mode.toLowerCase()}`} /><div className="mt-4 flex items-start gap-2 font-mono text-[10px] leading-5 text-muted"><ShieldCheck size={13} className="mt-0.5 shrink-0 text-green" /><span>Host mode uses your permissions. Use a supported sandbox, container, or VM to limit access.</span></div></div></div></section>

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

    <section className="section bg-panel/40"><div className="page-width grid gap-10 md:grid-cols-[1fr_1.15fr] md:gap-20"><div><Eyebrow><span className="mr-3 text-accent">04 /</span> Documentation</Eyebrow><h2 className="section-title mt-5">Start with<br />one workspace.</h2><p className="mt-5 max-w-sm text-sm leading-7 text-muted">Use the quickstart to start your first agent. The reference lists commands, API methods, and configuration settings.</p><Link to="/docs" className="text-link mt-7 text-xs">Open the docs <ArrowRight size={14} /></Link></div><div className="border-t border-line">{[{ icon: Terminal, title: 'Start your first workspace', description: 'Install Vibeke. Start an agent. Reconnect to the session.', slug: 'quickstart' }, { icon: BookOpen, title: 'How the runtime works', description: 'Agent processes, requests, and tasks.', slug: 'introduction' }, { icon: Code2, title: 'CLI reference', description: 'Commands and configuration.', slug: 'cli' }].map(({ icon: Icon, ...item }) => <Link key={item.slug} to="/docs/$slug" params={{ slug: item.slug }} className="group flex items-center gap-5 border-b border-line py-6"><Icon size={20} strokeWidth={1.5} className="shrink-0 text-muted group-hover:text-accent" /><div><h3 className="text-sm group-hover:text-accent">{item.title}</h3><p className="mt-2 text-xs text-muted">{item.description}</p></div><ChevronRight size={16} className="ml-auto shrink-0 text-muted group-hover:translate-x-1" /></Link>)}</div></div></section>

    <section className="page-width py-20 sm:py-24"><div className="grid gap-10 md:grid-cols-[1fr_1.5fr] md:gap-20"><div><Eyebrow>Before you install</Eyebrow><h2 className="mt-5 text-3xl font-medium tracking-tight">Common questions.</h2></div><div>{questions.map(([question, answer]) => <details key={question} className="group border-b border-line first:border-t"><summary className="flex min-h-16 list-none items-center justify-between gap-4 py-5 text-sm [&::-webkit-details-marker]:hidden">{question}<span className="font-mono text-lg text-muted group-open:rotate-45">+</span></summary><p className="max-w-xl pb-6 pr-8 text-sm leading-7 text-muted">{answer}</p></details>)}</div></div></section>

    <section className="border-t border-line bg-accent/[0.025]"><div className="page-width flex flex-col items-start justify-between gap-8 py-14 sm:flex-row sm:items-center"><div className="flex items-center gap-5"><img src="/brand/duck-128.png" width={80} height={80} alt="" className="hidden size-20 shrink-0 sm:block" /><div><h2 className="text-2xl font-medium tracking-tight sm:text-3xl">Try Vibeke in your next coding session.</h2><p className="mt-2 text-sm text-muted">Start with one project and your usual agent.</p></div></div><div className="shrink-0"><div className="flex flex-wrap gap-3"><a href="#install" className="button-primary" onClick={() => trackSiteEvent('install_intent', { placement: 'closing', target: 'first-use' })}>Install Vibeke <ArrowRight size={15} /></a><a href="https://github.com/vibeke-ai/vibeke" className="button-secondary" onClick={() => trackSiteEvent('github_outbound', { placement: 'closing', target: 'repository' })}><Star size={15} /> Star on GitHub</a></div><p className="mt-3 max-w-sm text-xs leading-6 text-muted">Want to come back later? Save the repository with a star.</p></div></div></section>
  </main>
}
