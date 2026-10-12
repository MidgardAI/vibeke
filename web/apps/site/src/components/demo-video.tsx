import { Play } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { trackSiteEvent } from '../lib/analytics'

export function DemoVideo() {
  const video = useRef<HTMLVideoElement>(null)
  const sent = useRef(new Set<number>())
  const [started, setStarted] = useState(false)
  const [error, setError] = useState(false)
  const [playing, setPlaying] = useState(false)
  const [ready, setReady] = useState(false)
  useEffect(() => {
    setReady(true)
  }, [])

  useEffect(() => { if (started) video.current?.focus() }, [started])

  async function play() {
    const element = video.current
    if (!element) return
    setError(false)
    try { await element.play() } catch { setError(true) }
  }

  return <figure id="demo" className="min-w-0 scroll-mt-24" aria-label="Terminal and phone approval preview">
    <div className="relative overflow-hidden rounded-sm border border-line bg-base">
      <video ref={video} className="block aspect-square w-full sm:aspect-video" controls={started} preload="none" playsInline
        tabIndex={started ? 0 : -1} aria-label="22-second narrated approval preview"
        onError={() => setError(true)}
        onPlay={() => {
          setStarted(true); setPlaying(true)
          if (!sent.current.has(0)) { sent.current.add(0); trackSiteEvent('demo_start', { placement: 'hero', target: 'quick-look' }) }
        }}
        onPause={() => setPlaying(false)}
        onEnded={() => { setPlaying(false); trackSiteEvent('demo_progress', { placement: 'hero', target: 'quick-look', quartile: 100 }); sent.current.clear() }}
        onTimeUpdate={event => {
          const element = event.currentTarget
          if (element.paused || element.seeking || !sent.current.has(0) || !Number.isFinite(element.duration)) return
          const quartile = Math.floor(element.currentTime / element.duration * 4) * 25
          if (quartile > 0 && quartile < 100 && !sent.current.has(quartile)) {
            sent.current.add(quartile); trackSiteEvent('demo_progress', { placement: 'hero', target: 'quick-look', quartile })
          }
        }}>
        <source src="/video/preview-v2-mobile.mp4" type="video/mp4" media="(max-width: 639px)" />
        <source src="/video/preview-v2-mobile.webm" type="video/webm" media="(max-width: 639px)" />
        <source src="/video/preview-v2.mp4" type="video/mp4" />
        <source src="/video/preview-v2.webm" type="video/webm" />
        <track kind="captions" src="/video/preview-v2.vtt" srcLang="en" label="English" />
      </video>
      {!started && <picture className="pointer-events-none absolute inset-0">
        <source media="(max-width: 639px)" srcSet="/video/preview-v2-mobile-poster.jpg" />
        <img src="/video/preview-v2-poster.jpg" alt="A paired phone shows an agent's request to run bun test, with an Allow button" width={1920} height={1080} fetchPriority="high" className="h-full w-full object-contain" />
      </picture>}
      {!started && <button type="button" disabled={!ready} onClick={() => void play()} className="group absolute inset-0 flex items-center justify-center bg-linear-to-t from-black/70 via-transparent to-transparent sm:items-end sm:pb-6" aria-label="Play 22-second narrated preview">
        <span className="inline-flex items-center gap-3 border border-accent/60 bg-base/95 px-5 py-3 font-mono text-xs text-cream group-hover:text-accent"><Play size={16} /> Watch it work · 22 sec</span>
      </button>}
    </div>
    <figcaption className="mt-3 flex flex-wrap items-center justify-between gap-2 text-xs leading-5 text-muted">
      <span>Narrated. Sample workspace with scripted agent output.</span>
      {started && <button type="button" className="text-accent underline underline-offset-4" onClick={() => playing ? video.current?.pause() : void play()}>{playing ? 'Pause preview' : 'Play preview'}</button>}
    </figcaption>
    {error && <p role="status" className="mt-2 text-sm text-muted">The preview could not play. <a className="text-accent underline" href="/video/preview-v2.mp4">Open the video</a> or read the example below.</p>}
    <details className="mt-1 text-xs leading-5 text-muted"><summary className="w-fit py-1">Read the preview</summary><p className="mt-2">Run Claude, Codex, or any agent in the terminal you already use. When a supported agent needs permission, the request waits in one inbox. Away from your desk? Approve it from your phone. The agent continues in your terminal. Your agents keep working, wherever you are.</p></details>
  </figure>
}
