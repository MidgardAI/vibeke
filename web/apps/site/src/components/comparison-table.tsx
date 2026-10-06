import { ArrowLeftRight, ArrowUpRight, ChevronDown } from 'lucide-react'
import { Fragment, useState } from 'react'
import { comparisonSections, type Product } from '../lib/comparisons'

export function ComparisonTable({ products, category }: { products: readonly Product[]; category: string }) {
  const [expanded, setExpanded] = useState<string | null>(null)
  const groups = comparisonSections(category)
  return <>
    <div className="mb-3 flex flex-wrap justify-between gap-2 font-mono text-[10px] text-muted"><p>Select a feature for details and sources.</p><p className="flex items-center gap-2 xl:hidden"><ArrowLeftRight size={12} /> Scroll to compare products.</p></div>
    <div className="overflow-x-auto border-y border-line [--feature-width:136px] sm:[--feature-width:180px]" style={{ containerType: 'inline-size' }} role="region" aria-label="Product comparison table" tabIndex={0}>
      <table className="w-full table-fixed border-separate border-spacing-0 text-left text-xs leading-5" style={{ minWidth: `calc(var(--feature-width) + ${products.length * 112}px)` }}>
        <caption className="sr-only">Vibeke and alternatives: feature comparison</caption>
        <colgroup><col style={{ width: 'var(--feature-width)' }} />{products.map(product => <col key={product.id} />)}</colgroup>
        <thead><tr>
          <th scope="col" className="sticky left-0 z-10 border-r border-b border-line bg-base px-3 py-5 font-mono text-[10px] font-normal uppercase tracking-widest text-muted">Feature</th>
          {products.map(product => <th key={product.id} scope="col" className={`border-b border-line px-3 py-5 font-mono text-xs font-medium ${product.id === 'vibeke' ? 'bg-accent/[0.07] text-accent' : 'bg-panel text-cream'}`}><a href={product.source} aria-label={`${product.name} documentation`} className="inline-flex items-center gap-1 hover:underline">{product.name}<ArrowUpRight size={10} /></a></th>)}
        </tr></thead>
        {groups.map(group => <tbody key={group.id}>
          <tr><th scope="rowgroup" colSpan={products.length + 1} className="border-b border-line bg-raised/60 px-3 py-2 font-mono text-[10px] font-normal uppercase tracking-widest text-green"><span className="sticky left-3">{group.label}</span></th></tr>
          {group.rows.map(row => <Fragment key={row.id}>
            <tr>
              <th scope="row" className="sticky left-0 z-10 border-r border-b border-line bg-base text-[11px] font-medium text-cream"><button type="button" className="flex min-h-12 w-full items-center justify-between gap-2 px-3 py-3 text-left hover:bg-panel hover:text-accent" aria-label={`Details: ${row.label}`} aria-expanded={expanded === row.id} aria-controls={`details-${row.id}`} onClick={() => setExpanded(expanded === row.id ? null : row.id)}>{row.label}<ChevronDown size={12} className={`shrink-0 transition-transform ${expanded === row.id ? 'rotate-180' : ''}`} /></button></th>
              {products.map(product => { const cell = row.cells[product.id]; return <td key={product.id} className={`border-b border-line px-3 py-3 text-[11px] ${product.id === 'vibeke' ? 'bg-accent/[0.035]' : ''} ${cell.state === 'yes' ? 'text-green' : cell.state === 'partial' ? 'text-accent' : cell.state === 'no' ? 'text-muted' : 'text-cream'}`}>{cell.label}</td> })}
            </tr>
            {expanded === row.id && <tr><td colSpan={products.length + 1} className="border-b border-line bg-panel p-0"><div id={`details-${row.id}`} role="region" aria-label={`${row.label} details`} className="sticky left-0 grid w-[100cqi] grid-cols-1 gap-x-6 gap-y-5 p-5 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5">{products.map(product => <div key={product.id} className="min-w-0"><h3 className={`font-mono text-[11px] ${product.id === 'vibeke' ? 'text-accent' : 'text-cream'}`}>{product.name}</h3><p className="mt-2 text-xs leading-6 text-muted">{row.cells[product.id].detail}</p><a href={row.cells[product.id].source} aria-label={`${product.name} source: ${row.label}`} className="mt-2 inline-flex items-center gap-1 text-[10px] text-accent hover:underline">Source <ArrowUpRight size={10} /></a></div>)}</div></td></tr>}
          </Fragment>)}
        </tbody>)}
      </table>
    </div>
  </>
}
