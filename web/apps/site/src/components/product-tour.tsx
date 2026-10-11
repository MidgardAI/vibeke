import { Play } from 'lucide-react'
import { useRef, useState } from 'react'
import { Eyebrow } from './ui'

const chapters = [
  ['0:07', 'Terminal'],
  ['0:22', 'Inbox'],
  ['0:32', 'Remote preview'],
  ['1:02', 'Handoff'],
  ['1:21', 'Desktop app'],
  ['1:31', 'Menu bar'],
  ['1:39', 'Pair a phone'],
  ['2:09', 'Share'],
] as const

const seconds = (time: string) => { const [m, s] = time.split(':').map(Number); return m! * 60 + s! }

export function ProductTour() {
  const video = useRef<HTMLVideoElement>(null)
  const [started, setStarted] = useState(false)

  function play(at?: string) {
    const v = video.current
    if (!v) return
    if (at) v.currentTime = seconds(at)
    setStarted(true)
    void v.play()
  }

  return <section id="tour" aria-labelledby="tour-title" className="border-b border-line">
    <div className="page-width py-16 sm:py-20">
      <div className="grid gap-6 md:grid-cols-2">
        <div><Eyebrow><span className="mr-3 text-accent">~/</span> Product tour · 2:41</Eyebrow><h2 id="tour-title" className="section-title mt-5">See it at work.</h2></div>
        <p className="max-w-md self-end text-sm leading-7 text-muted md:ml-auto">Watch agents work in the terminal on two machines. Preview a dev server. Hand work to another computer. Answer requests from the desktop app and a phone. The video uses sample data.</p>
      </div>
      <div className="relative mt-10 overflow-hidden border border-line bg-base">
        <video ref={video} className="block aspect-video w-full" controls={started} preload="none" playsInline poster="/video/vibeke-tour-poster.jpg"
          onPlay={() => setStarted(true)} aria-label="Vibeke product tour">
          <source src="/video/vibeke-tour.mp4" type="video/mp4" />
          <track kind="captions" src="/video/vibeke-tour.vtt" srcLang="en" label="English" />
        </video>
        {!started && <button type="button" onClick={() => play()} className="group absolute inset-0 flex items-center justify-center bg-base/30 transition-colors hover:bg-base/10" aria-label="Play the product tour">
          <span className="flex items-center gap-3 border border-accent/50 bg-base/90 px-6 py-4 font-mono text-xs text-cream shadow-2xl transition group-hover:border-accent group-hover:text-accent"><Play size={18} className="fill-current text-accent" /> Play the tour</span>
        </button>}
      </div>
      <div className="mt-4 flex flex-wrap gap-x-5 gap-y-2 font-mono text-[11px] text-muted" aria-label="Chapters">
        {chapters.map(([at, title]) => <button key={at} type="button" onClick={() => play(at)} className="inline-flex min-h-8 items-center gap-2 hover:text-cream"><span className="text-accent">{at}</span>{title}</button>)}
      </div>
    </div>
  </section>
}
