import { useEffect, useRef, useState } from 'react'
import { Link, useRouterState } from '@tanstack/react-router'
import { ArrowUpRight, FileText, Search, X } from 'lucide-react'
import { docs } from 'virtual:vibeke-docs'
import { searchDocs } from '../lib/search'

export function SearchDialog() {
  const dialog = useRef<HTMLDialogElement>(null)
  const input = useRef<HTMLInputElement>(null)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState(0)
  const pathname = useRouterState({ select: state => state.location.pathname })
  const results = searchDocs(docs, query)
  function open() { setQuery(''); setSelected(0); dialog.current?.showModal(); input.current?.focus() }
  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
        event.preventDefault()
        if (dialog.current?.open) dialog.current.close()
        else open()
      }
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [])
  useEffect(() => { dialog.current?.close() }, [pathname])
  return <>
    <button type="button" onClick={open} className="flex h-9 items-center gap-3 border border-line px-3 text-muted transition hover:border-muted hover:text-cream" aria-label="Search documentation">
      <Search size={14} /><span className="hidden text-xs lg:inline">Search docs</span><kbd className="hidden font-mono text-[10px] sm:inline">⌘ K</kbd>
    </button>
    <dialog ref={dialog} className="search-dialog" aria-labelledby="search-title" onClick={event => { if (event.target === dialog.current) dialog.current.close() }}>
      <div className="border border-line bg-panel shadow-2xl">
        <div className="flex items-center gap-3 border-b border-line p-5">
          <Search size={19} className="text-accent" /><label id="search-title" htmlFor="docs-search" className="sr-only">Search documentation</label>
          <input ref={input} id="docs-search" value={query} onChange={event => { setQuery(event.target.value); setSelected(0) }} placeholder="Find a command, concept, or guide…" autoComplete="off" className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted" aria-controls="search-results" aria-describedby="search-help" onKeyDown={event => {
            if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
              event.preventDefault()
              setSelected(value => Math.max(0, Math.min(results.length - 1, value + (event.key === 'ArrowDown' ? 1 : -1))))
            }
            if (event.key === 'Enter') {
              // Closing the dialog restores focus to its trigger. Cancel Enter's
              // default activation so that trigger does not reopen it on keyup.
              event.preventDefault()
              dialog.current?.querySelector<HTMLAnchorElement>(`[data-result="${selected}"]`)?.click()
            }
          }} />
          <button type="button" className="p-1 text-muted hover:text-cream" onClick={() => dialog.current?.close()} aria-label="Close search"><X size={19} /></button>
        </div>
        <div className="max-h-[55vh] overflow-y-auto p-3" id="search-results">
          <p className="px-3 py-2 font-mono text-[10px] uppercase tracking-widest text-muted">{query ? `${results.length} results` : 'Common pages'}</p>
          {results.length ? results.map((doc, i) => <Link key={doc.slug} to="/docs/$slug" params={{ slug: doc.slug }} data-result={i} onClick={() => dialog.current?.close()} className={`group flex items-start gap-3 px-3 py-3 transition hover:bg-raised ${selected === i ? 'bg-raised' : ''}`}>
            <FileText size={16} className="mt-1 shrink-0 text-accent" /><div className="min-w-0 flex-1"><p className="text-sm text-cream">{doc.title}<span className="ml-3 font-mono text-[10px] text-muted">{doc.group}</span></p><p className="mt-1 text-xs leading-5 text-muted">{doc.description}</p></div><ArrowUpRight size={15} className="mt-1 shrink-0 text-muted" />
          </Link>) : <p className="px-3 py-8 text-sm text-muted">No matches for “{query}”. Try “workspace”, “approval”, or “SSH”.</p>}
        </div>
        <div id="search-help" className="flex justify-between border-t border-line px-5 py-3 font-mono text-[10px] text-muted"><span>↑ ↓ navigate · ↵ open</span><span>esc to close</span></div>
      </div>
    </dialog>
  </>
}
