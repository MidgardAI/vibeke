import { Link, useRouterState } from '@tanstack/react-router'
import { ArrowRight, ArrowUpRight, Github, Menu, X } from 'lucide-react'
import { useEffect, useState } from 'react'
import { SearchDialog } from './search'

export function Brand() {
  return <Link to="/" className="flex shrink-0 items-center gap-2.5 text-cream" aria-label="Vibeke home"><img src="/brand/duck-128.png" srcSet="/brand/duck-64.png 1x, /brand/duck-128.png 2x" width={48} height={48} alt="" className="size-12" /><span className="font-mono text-xl font-semibold tracking-[-0.06em]">vibeke</span></Link>
}

export function Header() {
  const [open, setOpen] = useState(false)
  const pathname = useRouterState({ select: state => state.location.pathname })
  useEffect(() => { setOpen(false) }, [pathname])
  return <header className="site-header sticky top-0 z-40 border-b border-line bg-base/95 backdrop-blur-md">
    <div className="page-width flex h-[76px] items-center gap-6">
      <Brand />
      <nav aria-label="Main navigation" className="ml-7 hidden items-center gap-7 text-xs md:flex">
        <Link to="/" activeOptions={{ exact: true }} activeProps={{ className: 'text-cream' }} className="nav-link">Overview</Link>
        <Link to="/docs" activeProps={{ className: 'text-cream' }} className="nav-link">Documentation</Link>
        <Link to="/compare" activeProps={{ className: 'text-cream' }} className="nav-link">Compare</Link>
      </nav>
      <div className="ml-auto flex items-center gap-3"><SearchDialog /><a href="https://github.com/MidgardAI/vibeke" aria-label="Vibeke on GitHub" className="hidden p-2 text-muted hover:text-cream sm:block"><Github size={18} /></a><Link to="/docs/$slug" params={{ slug: 'install' }} className="button-primary hidden min-h-9 px-4 text-xs lg:inline-flex">Install Vibeke <ArrowUpRight size={14} /></Link><button type="button" className="p-2 md:hidden" aria-label={open ? 'Close navigation' : 'Open navigation'} aria-expanded={open} aria-controls="mobile-navigation" onClick={() => setOpen(!open)}>{open ? <X size={21} /> : <Menu size={21} />}</button></div>
    </div>
    {open && <nav id="mobile-navigation" aria-label="Mobile navigation" className="page-width flex flex-col gap-1 border-t border-line py-4 text-sm md:hidden"><Link className="py-3" to="/">Overview</Link><Link className="py-3" to="/docs">Documentation</Link><Link className="py-3" to="/compare">Compare</Link><Link className="py-3 text-accent" to="/docs/$slug" params={{ slug: 'install' }}>Install Vibeke →</Link></nav>}
  </header>
}

export function Footer() {
  return <footer className="border-t border-line">
    <div className="page-width flex flex-col justify-between gap-8 py-10 sm:flex-row sm:items-center">
      <div><Brand /><p className="mt-3 font-mono text-[10px] text-muted">Terminal workspaces for coding agents.</p></div>
      <div className="flex flex-wrap gap-x-7 gap-y-4 text-xs text-muted"><Link to="/docs" className="hover:text-cream">Documentation</Link><Link to="/compare" className="hover:text-cream">Compare</Link><Link to="/docs/$slug" params={{ slug: 'security' }} className="hover:text-cream">Security</Link><a href="https://github.com/MidgardAI/vibeke" className="flex items-center gap-1 hover:text-cream">GitHub <ArrowUpRight size={12} /></a></div>
    </div>
    <div className="page-width flex flex-wrap justify-between gap-3 border-t border-line py-5 font-mono text-[10px] text-muted"><span>Rust · macOS · Linux</span><span>Apache-2.0</span></div>
  </footer>
}

export function NotFound() {
  return <main id="main" className="page-width py-28"><p className="font-mono text-sm text-accent">404 · path not found</p><h1 className="mt-6 text-5xl tracking-tight">Page not found.</h1><p className="mt-5 text-muted">Check the URL or find the page in the documentation.</p><Link to="/docs" className="button-primary mt-8">Open documentation <ArrowRight size={16} /></Link></main>
}
