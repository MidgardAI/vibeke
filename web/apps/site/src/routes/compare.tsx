import { createFileRoute, Link } from '@tanstack/react-router'
import { ArrowRight } from 'lucide-react'
import { useState } from 'react'
import { ComparisonTable } from '../components/comparison-table'
import { Eyebrow } from '../components/ui'
import { comparisonCount, comparisonDate, comparisonGroups, comparisonSections, products } from '../lib/comparisons'

export const Route = createFileRoute('/compare')({
  head: () => ({ meta: [
    { title: 'Compare Vibeke and other agent tools' },
    { name: 'description', content: 'Compare agent control, file transfers, browser tools, task review, and process recovery across ten products.' },
  ] }),
  component: Compare,
})

function Compare() {
  const [filter, setFilter] = useState<'all' | 'terminal' | 'app'>('all')
  const [category, setCategory] = useState('all')
  const selected = products.filter(product => product.id === 'vibeke' || filter === 'all' || product.kind === filter)
  const count = comparisonSections(category).reduce((sum, group) => sum + group.rows.length, 0)
  return <main id="main">
    <section className="page-width max-w-[1440px] pt-16 pb-10 sm:pt-20">
      <Eyebrow>~/ compare</Eyebrow>
      <h1 className="mt-6 max-w-3xl text-5xl leading-[1.06] font-medium tracking-[-0.05em] sm:text-6xl">Compare the tools.</h1>
      <p className="mt-6 max-w-2xl text-base leading-8 text-muted">Compare where agents run, how you control them, and how you review their work.</p>
    </section>
    <section id="features" className="page-width max-w-[1440px] scroll-mt-28 pb-20" aria-label="Product comparison">
      <div className="border-t border-line pt-5">
        <div className="flex flex-wrap gap-2" role="group" aria-label="Filter products">
          {([['all', 'All tools'], ['terminal', 'Terminal tools'], ['app', 'Agent apps']] as const).map(([value, label]) => <button key={value} type="button" onClick={() => setFilter(value)} aria-pressed={filter === value} className={`min-h-10 border px-4 font-mono text-[10px] ${filter === value ? 'border-accent/40 bg-accent/10 text-accent' : 'border-line text-muted hover:text-cream'}`}>{label}</button>)}
        </div>
        <div className="mt-3 flex flex-wrap gap-2" role="group" aria-label="Filter features">
          {[{ id: 'all', label: `All ${comparisonCount} features` }, ...comparisonGroups].map(group => <button key={group.id} type="button" aria-pressed={category === group.id} onClick={() => setCategory(group.id)} className={`min-h-10 border px-3 font-mono text-[10px] ${category === group.id ? 'border-accent/40 bg-accent/10 text-accent' : 'border-transparent text-muted hover:text-cream'}`}>{group.label}</button>)}
        </div>
        <div className="my-5 flex flex-wrap justify-between gap-2 font-mono text-[10px] text-muted"><span aria-live="polite">{selected.length} products · {count} features</span><span>Checked {comparisonDate}</span></div>
      </div>
      <ComparisonTable products={selected} category={category} />
      <div className="mt-5 grid gap-3 text-xs leading-6 text-muted md:grid-cols-2 md:gap-12">
        <p>Agent support and isolation depend on your setup. A transfer between hosts copies work and conversation data. It does not move a live process.</p>
      </div>
      <div className="mt-10 flex flex-wrap gap-6 border-t border-line pt-6"><Link to="/docs/$slug" params={{ slug: 'install' }} className="text-link text-xs">Install Vibeke <ArrowRight size={14} /></Link><Link to="/docs/$slug" params={{ slug: 'handoff' }} className="text-link text-xs">Transfer and share work <ArrowRight size={14} /></Link></div>
    </section>
  </main>
}
