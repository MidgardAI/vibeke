import { Check, Copy } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'

export function CopyButton({ text, label = 'Copy command', className = '', onCopied }: { text: string; label?: string; className?: string; onCopied?: () => void }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle')
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  useEffect(() => () => clearTimeout(timer.current), [])
  async function copy() {
    try {
      await navigator.clipboard.writeText(text)
      setState('copied')
      onCopied?.()
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

export function CodeBlock({ code, title = 'terminal', onCopied }: { code: string; title?: string; onCopied?: () => void }) {
  return <div className="overflow-hidden border border-line bg-base">
    <div className="flex items-center justify-between border-b border-line px-5 py-3 font-mono text-xs text-muted"><span>{title}</span><CopyButton text={code} onCopied={onCopied} /></div>
    <pre className="overflow-x-auto p-5 font-mono text-[13px] leading-7 text-cream"><code>{code.split('\n').map((line, i) => <span key={i} className={`block min-h-7 ${line.startsWith('#') ? 'text-muted' : ''}`}>{line || ' '}</span>)}</code></pre>
  </div>
}

/** The GitHub mark. lucide-react no longer ships brand icons. */
export function GithubIcon({ size = 24 }: { size?: number }) {
  return <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 .5a11.5 11.5 0 0 0-3.64 22.41c.58.1.79-.25.79-.56v-2c-3.2.7-3.88-1.37-3.88-1.37-.52-1.33-1.28-1.69-1.28-1.69-1.05-.71.08-.7.08-.7 1.16.08 1.77 1.19 1.77 1.19 1.03 1.77 2.7 1.26 3.36.96.1-.75.4-1.26.73-1.55-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.29 1.19-3.1-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.17 1.18a11 11 0 0 1 5.77 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.84 1.19 3.1 0 4.42-2.69 5.39-5.26 5.68.41.36.78 1.06.78 2.14v3.17c0 .31.21.67.8.56A11.5 11.5 0 0 0 12 .5Z" /></svg>
}
