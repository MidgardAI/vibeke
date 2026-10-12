import { Play } from 'lucide-react'
import { useEffect, useRef, useState } from 'react'
import { trackSiteEvent } from '../lib/analytics'

export function DemoVideo() {
  const video = useRef<HTMLVideoElement>(null)
  const sent = useRef(new Set<number>())
  const [started, setStarted] = useState(false)
  const [error, setError] = useState(false)
  const [playing, setPlaying] = useState(false)
  useEffect(() => {
    if (video.current && window.matchMedia('(max-width: 639px)').matches) video.current.poster = '/video/quick-look-mobile-poster.jpg'
  }, [])

  async function play() {
    const element = video.current
    if (!element) return
    setError(false)
    try { await element.play() } catch { setError(true) }
  }

  return <figure id="demo" className="min-w-0 scroll-mt-24" aria-label="Terminal and phone approval preview">
    <div className="relative overflow-hidden rounded-sm border border-line bg-base">
      <video ref={video} className="block aspect-square w-full sm:aspect-video" controls={started} preload="none" playsInline muted
        poster="/video/quick-look-poster.jpg" aria-label="20-second approval preview"
        onError={() => setError(true)}
        onPlay={() => {
          setStarted(true); setPlaying(true)
          if (!sent.current.has(0)) { sent.current.add(0); trackSiteEvent('demo_start', { placement: 'hero', target: 'quick-look' }) }
        }}
        onPause={() => setPlaying(false)}
        onEnded={() => { setPlaying(false); trackSiteEvent('demo_progress', { placement: 'hero', target: 'quick-look', quartile: 100 }); sent.current.clear() }}
        onTimeUpdate={event => {
          const element = event.currentTarget
          if (element.seeking || !Number.isFinite(element.duration)) return
          const quartile = Math.floor(element.currentTime / element.duration * 4) * 25
          if (quartile > 0 && quartile < 100 && !sent.current.has(quartile)) {
            sent.current.add(quartile); trackSiteEvent('demo_progress', { placement: 'hero', target: 'quick-look', quartile })
          }
        }}>
        <source src="/video/quick-look-mobile.mp4" type="video/mp4" media="(max-width: 639px)" />
        <source src="/video/quick-look.mp4" type="video/mp4" />
        <track kind="captions" src="/video/quick-look.vtt" srcLang="en" label="English" default />
      </video>
      {!started && <button type="button" onClick={() => void play()} className="group absolute inset-0 flex items-center justify-center bg-linear-to-t from-black/70 via-transparent to-transparent sm:items-end sm:pb-6" aria-label="Play 20-second preview">
        <span className="inline-flex items-center gap-3 border border-accent/60 bg-base/95 px-5 py-3 font-mono text-xs text-cream group-hover:text-accent"><Play size={16} /> Watch it work · 20 sec</span>
      </button>}
    </div>
    <figcaption className="mt-3 flex flex-wrap items-center justify-between gap-2 text-xs leading-5 text-muted">
      <span>Terminal and phone approvals. Sample workspace.</span>
      {started && <button type="button" className="text-accent underline underline-offset-4" onClick={() => playing ? video.current?.pause() : void play()}>{playing ? 'Pause preview' : 'Play preview'}</button>}
    </figcaption>
    {error && <p role="status" className="mt-2 text-sm text-muted">The preview could not play. <a className="text-accent underline" href="/video/quick-look.mp4">Open the video</a> or read the example below.</p>}
    <details className="mt-1 text-xs leading-5 text-muted"><summary className="w-fit py-1">Read the preview</summary><p className="mt-2">An agent asks to run tests in the terminal inbox. The user allows it. In a second example, a paired phone shows a request to push a branch. The user allows the request, then writes another instruction.</p></details>
  </figure>
}
