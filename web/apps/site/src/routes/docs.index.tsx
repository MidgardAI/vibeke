import { createFileRoute, Link } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight, BookOpen, Code2, Terminal } from 'lucide-react'
import { docs } from 'virtual:vibeke-docs'
import { DocsLayout } from '../components/docs'
import { CodeBlock, Eyebrow } from '../components/ui'

export const Route = createFileRoute('/docs/')({
  head: () => ({ meta: [{ title: 'Documentation — Vibeke' }, { name: 'description', content: 'Install Vibeke. Configure agents, tasks, and remote previews. Read the CLI and API references.' }] }),
  component: DocsIndex,
})

function DocsIndex() {
  return <DocsLayout><Eyebrow>~/ docs</Eyebrow><h1 className="mt-5 text-4xl font-medium tracking-[-0.045em] sm:text-5xl">Vibeke documentation.</h1><p className="mt-5 text-base leading-8 text-muted">Install Vibeke. Start an agent. Reconnect to its session. Use the guides for setup. Use the reference for commands and API methods.</p><div className="mt-8 grid gap-3 sm:grid-cols-2">{[{ icon: Terminal, title: 'Start here', text: 'Install Vibeke and open a workspace.', slug: 'install' }, { icon: Code2, title: 'Find a command', text: 'Find commands and arguments.', slug: 'cli' }].map(({ icon: Icon, ...item }) => <Link key={item.slug} to="/docs/$slug" params={{ slug: item.slug }} className="group border border-line bg-panel/50 p-5 hover:border-accent/40"><Icon size={20} className="text-accent" /><h2 className="mt-5 flex items-center justify-between text-sm">{item.title}<ArrowUpRight size={14} className="text-muted group-hover:text-accent" /></h2><p className="mt-2 text-xs leading-6 text-muted">{item.text}</p></Link>)}</div><div className="mt-10"><CodeBlock title="your first session" code={'vibeke\n\n# From another shell, in your project\nvibeke workspace create .\nvibeke agent start builder --harness claude'} /><Link className="text-link mt-4 text-xs" to="/docs/$slug" params={{ slug: 'quickstart' }}>Read the quickstart <ArrowRight size={13} /></Link></div><h2 className="mt-14 border-b border-line pb-5 text-xl tracking-tight">Guides and reference</h2>{['Core concepts', 'Guides', 'Reference'].map(group => <section key={group} className="mt-8"><Eyebrow>{group}</Eyebrow><div className="mt-3 divide-y divide-line">{docs.filter(doc => doc.group === group).map(doc => <Link key={doc.slug} to="/docs/$slug" params={{ slug: doc.slug }} className="group flex items-start gap-3 py-4"><BookOpen size={14} className="mt-1 shrink-0 text-muted group-hover:text-accent" /><div><h3 className="text-sm group-hover:text-accent">{doc.title}</h3><p className="mt-1 text-xs leading-6 text-muted">{doc.description}</p></div><ArrowUpRight size={13} className="mt-1 ml-auto shrink-0 text-muted" /></Link>)}</div></section>)}</DocsLayout>
}
