import { Check, Copy } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'

export function CopyButton({ text, label = 'Copy command', className = '' }: { text: string; label?: string; className?: string }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle')
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  useEffect(() => () => clearTimeout(timer.current), [])
  async function copy() {
    try {
      await navigator.clipboard.writeText(text)
      setState('copied')
    } catch { setState('failed') }
    clearTimeout(timer.current)
    timer.current = setTimeout(() => setState('idle'), 2400)
  }
  return <button type="button" onClick={copy} className={`copy-button ${className}`} aria-label={state === 'copied' ? 'Copied' : label} title={state === 'failed' ? 'The clipboard is unavailable. Select the text. Use your browser copy command.' : label}>
    {state === 'copied' ? <Check size={15} /> : <Copy size={15} />}
    <span className="sr-only" role="status">{state === 'copied' ? 'Copied to clipboard' : state === 'failed' ? 'The clipboard is unavailable. Select the text. Use your browser copy command.' : ''}</span>
  </button>
}

export function Eyebrow({ children, className = '' }: { children: ReactNode; className?: string }) {
  return <div className={`font-mono text-[11px] uppercase tracking-[0.16em] text-muted ${className}`}>{children}</div>
}

export function CodeBlock({ code, title = 'terminal' }: { code: string; title?: string }) {
  return <div className="overflow-hidden border border-line bg-base">
    <div className="flex items-center justify-between border-b border-line px-5 py-3 font-mono text-xs text-muted"><span>{title}</span><CopyButton text={code} /></div>
    <pre className="overflow-x-auto p-5 font-mono text-[13px] leading-7 text-cream"><code>{code.split('\n').map((line, i) => <span key={i} className={`block min-h-7 ${line.startsWith('#') ? 'text-muted' : ''}`}>{line || ' '}</span>)}</code></pre>
  </div>
}
