import { Expand, Globe2, Monitor, Terminal, X } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'

const clients = [
  { id: 'tui', label: 'Terminal', kind: 'TUI', icon: Terminal, width: 1440, height: 900, description: 'Run agents in your terminal.', alt: 'Vibeke terminal with workspaces and agent output from a session recovery task.' },
  { id: 'electron', label: 'Desktop', kind: 'Electron app', icon: Monitor, width: 2880, height: 1740, description: 'Read the conversation. Review the changes.', alt: 'Vibeke desktop app with a workspace list, agent conversation, and changed files.' },
  { id: 'web', label: 'Browser', kind: 'Web app', icon: Globe2, width: 860, height: 1720, description: 'Answer agent requests from your browser.', alt: 'Vibeke web app on a phone, with requests to run tests and build the website.' },
] as const

export function ClientShowcase() {
  const [selected, setSelected] = useState(1)
  const [ready, setReady] = useState(false)
  const [expanded, setExpanded] = useState(false)
  const dialog = useRef<HTMLDialogElement>(null)
  const current = clients[selected]!

  useEffect(() => { setReady(true) }, [])

  useEffect(() => {
    if (!expanded) return
    const previous = document.body.style.overflow
    document.body.style.overflow = 'hidden'
    return () => { document.body.style.overflow = previous }
  }, [expanded])

  function enlarge() {
    dialog.current?.showModal()
    setExpanded(true)
  }

  return <section aria-label="Vibeke screenshots" className="client-showcase">
    <div className="mb-2 flex flex-wrap items-center justify-between gap-2 font-mono text-[10px] text-muted">
      <span><span className="mr-2 text-accent">~/</span> One session. Three ways to connect.</span>
      <span className="hidden sm:inline">TUI / Electron / Web</span>
    </div>
    <div id="client-panel" role="tabpanel" aria-labelledby={`client-tab-${current.id}`}>
      <div className="client-orbit" data-active-client={current.id}>
        <svg className="client-orbit-track" viewBox="0 0 1160 560" preserveAspectRatio="none" aria-hidden="true">
          <ellipse cx="580" cy="295" rx="551" ry="200" />
          <ellipse cx="580" cy="295" rx="515" ry="172" />
          <path d="M580 510v18M29 295H9M1131 295h20" />
        </svg>
        {clients.map((client, index) => {
          const position = index === selected ? 'front' : (index - selected + 3) % 3 === 1 ? 'right' : 'left'
          const Icon = client.icon
          return <button key={client.id} type="button" disabled={!ready} tabIndex={-1} aria-hidden={index !== selected}
            aria-label={`Enlarge ${client.kind} screenshot`}
            onClick={() => index === selected ? enlarge() : setSelected(index)}
            className={`client-frame ${client.id === 'web' ? 'client-frame-phone' : ''}`}
            data-position={position} data-client={client.id}>
            <span className="client-frame-title"><span className="flex items-center gap-2"><Icon size={11} />{client.kind}</span><Expand size={11} /></span>
            <img src={`/screenshots/${client.id}.png`} width={client.width} height={client.height} alt={client.alt} fetchPriority={client.id === 'electron' ? 'high' : 'auto'} draggable={false} />
          </button>
        })}
      </div>
      <p className="mt-2 min-h-6 text-center text-sm text-muted" aria-live="polite">{current.description}</p>
    </div>
    <div className="mt-5 flex justify-center" role="tablist" aria-label="Choose an app screenshot">
      {clients.map((client, index) => {
        const Icon = client.icon
        return <button key={client.id} type="button" disabled={!ready} role="tab" id={`client-tab-${client.id}`} aria-controls="client-panel"
          aria-selected={selected === index} tabIndex={selected === index ? 0 : -1}
          onClick={() => setSelected(index)} onKeyDown={event => {
            if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return
            event.preventDefault()
            const next = event.key === 'Home' ? 0 : event.key === 'End' ? 2 : (selected + (event.key === 'ArrowRight' ? 1 : 2)) % 3
            setSelected(next)
            document.getElementById(`client-tab-${clients[next]!.id}`)?.focus()
          }} className={`flex min-h-11 items-center gap-2 border-y px-4 font-mono text-[11px] transition-colors first:border-l last:border-r sm:px-6 ${selected === index ? 'border-accent/50 bg-accent/10 text-accent' : 'border-line bg-base text-muted hover:text-cream'}`}>
          <Icon size={13} />{client.label}
        </button>
      })}
    </div>
    <div className="mt-4 flex flex-wrap items-center justify-center gap-x-4 gap-y-2 font-mono text-[10px] text-muted">
      <span>Screenshots with sample data.</span><span aria-hidden="true" className="text-line">/</span>
      <button type="button" disabled={!ready} onClick={enlarge} className="inline-flex min-h-8 items-center gap-2 text-cream hover:text-accent"><Expand size={12} />View full size</button>
    </div>
    <dialog ref={dialog} aria-labelledby="screenshot-title" className="client-dialog" onClose={() => setExpanded(false)} onClick={event => { if (event.target === event.currentTarget) dialog.current?.close() }}>
      <div className="client-dialog-content">
        <div className="flex items-center justify-between gap-4 border-b border-line bg-panel px-4 py-2">
          <div className="font-mono text-xs"><span id="screenshot-title">{current.kind}</span><span className="ml-3 hidden text-[10px] text-muted sm:inline">Sample workspace</span></div>
          <button type="button" autoFocus aria-label="Close screenshot" onClick={() => dialog.current?.close()} className="flex h-9 w-9 items-center justify-center text-muted hover:text-cream"><X size={19} /></button>
        </div>
        <div className="client-dialog-image"><img src={`/screenshots/${current.id}.png`} width={current.width} height={current.height} alt={current.alt} /></div>
      </div>
    </dialog>
  </section>
}
