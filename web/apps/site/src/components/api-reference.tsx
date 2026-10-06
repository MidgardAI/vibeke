import { Fragment, useState } from 'react'
import { ChevronDown, Search } from 'lucide-react'
import { apiMethods } from 'virtual:vibeke-docs'
import { CopyButton } from './ui'

const groups = [...new Set(apiMethods.map(method => method.name.split('.')[0]!))]

export function ApiReference() {
  const [query, setQuery] = useState('')
  const [group, setGroup] = useState('all')
  const [limit, setLimit] = useState(20)
  const [open, setOpen] = useState<string | null>(null)
  const matches = apiMethods.filter(method => (group === 'all' || method.name.startsWith(group + '.')) && method.name.includes(query.trim().toLowerCase()))
  return <section aria-labelledby="methods">
    <h2 id="methods">Methods</h2>
    <p>Find a method. Select its name to see the parameters and result.</p>
    <div className="flex flex-col gap-3 sm:flex-row">
      <label className="flex min-w-0 flex-1 items-center gap-2 border border-line bg-panel px-3 focus-within:border-accent"><Search size={15} aria-hidden="true" /><input type="search" aria-label="Find an API method" placeholder="agent.start" value={query} onChange={event => { setQuery(event.target.value); setLimit(20) }} className="min-h-11 min-w-0 flex-1 bg-transparent font-mono text-xs text-cream outline-none" /></label>
      <select aria-label="Method group" value={group} onChange={event => { setGroup(event.target.value); setLimit(20) }} className="min-h-11 border border-line bg-panel px-3 font-mono text-xs text-cream"><option value="all">All groups</option>{groups.map(name => <option key={name} value={name}>{name}</option>)}</select>
    </div>
    <p role="status" className="!my-3 font-mono text-[10px]">{matches.length} {matches.length === 1 ? 'method' : 'methods'}{matches.length > limit ? ` · showing ${limit}` : ''}</p>
    <div className="overflow-hidden border border-line">
      <table className="table-fixed" aria-label="API methods">
        <colgroup><col className="w-[64%] sm:w-[65%]" /><col /></colgroup>
        <thead><tr><th scope="col">Method</th><th scope="col">Access</th></tr></thead>
        <tbody>{matches.slice(0, limit).map(method => <Fragment key={method.name}>
          <tr><th scope="row" className="!bg-transparent !p-0"><button type="button" aria-expanded={open === method.name} aria-controls={`api-${method.name}`} onClick={() => setOpen(open === method.name ? null : method.name)} className="flex w-full items-center justify-between gap-2 px-3 py-3 text-left hover:bg-panel sm:px-4"><span className="min-w-0 font-mono text-[11px] break-all">{method.name}</span><ChevronDown size={13} className={`shrink-0 ${open === method.name ? 'rotate-180' : ''}`} /></button></th><td className="!px-3 !py-3 text-[11px] sm:!px-4">{method.pane_scope === 'forbidden' ? 'Full' : method.pane_scope === 'own_target' ? 'Own panes' : 'Pane'}</td></tr>
          {open === method.name && <tr><td colSpan={2} className="!p-0"><div id={`api-${method.name}`} role="region" aria-label={`${method.name} details`} className="min-w-0 bg-panel p-4 sm:p-5"><p className="!mt-0 text-xs">{method.mutating ? 'This method can change state.' : 'This method reads data.'}</p>{([['Parameters', method.params], ['Result', method.result]] as const).map(([label, shape]) => <div key={label} className="mt-4"><div className="mb-2 flex items-center justify-between"><h3 className="!m-0 !text-xs">{label}</h3><CopyButton text={shape} label={`Copy ${label.toLowerCase()}`} /></div><pre className="!whitespace-pre-wrap !break-words !border !border-line !bg-base !p-3"><code>{shape}</code></pre></div>)}</div></td></tr>}
        </Fragment>)}</tbody>
      </table>
      {matches.length === 0 && <p className="px-4 text-sm">No methods match. Change the search or group.</p>}
    </div>
    {matches.length > limit && <button type="button" className="mt-4 min-h-11 border border-line px-4 font-mono text-xs text-accent hover:bg-panel" onClick={() => setLimit(limit + 20)}>Show more methods</button>}
  </section>
}
