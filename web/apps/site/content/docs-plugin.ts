import { readFileSync } from 'node:fs'
import { dirname, relative, resolve, sep } from 'node:path'
import { fileURLToPath } from 'node:url'
import GithubSlugger from 'github-slugger'
import type { Plugin } from 'vite'
import { docManifest, type Doc } from './manifest.ts'

const repoRoot = fileURLToPath(new URL('../../../../', import.meta.url))
const docsRoot = resolve(repoRoot, 'docs')
const bookRoot = resolve(docsRoot, 'site/src')
const virtualId = 'virtual:vibeke-docs'
const resolvedId = '\0' + virtualId
const apiFiles = ['methods.json', 'vibeke-1.schema.json']
const sourceRoutes = new Map(docManifest.map(doc => [resolve(bookRoot, doc.file), '/docs/' + doc.slug]))

function expandMarkdown(file: string, stack: string[] = []): string {
  if (!file.startsWith(docsRoot + sep) || stack.includes(file)) {
    throw new Error(`Invalid or circular documentation include: ${file}`)
  }
  const body = readFileSync(file, 'utf8')
    .replace(/<!--[^]*?-->/g, '')
    .replace(/\]\(([^\s)]+)(\s+"[^"]*")?\)/g, (match, href: string, title = '') => {
      if (/^(?:[a-z]+:|#|\/)/i.test(href)) return match
      const [path, hash] = href.split('#')
      const target = resolve(dirname(file), path!)
      const route = (apiFiles.includes(path!.split('/').at(-1)!) ? '/api-reference/' + path!.split('/').at(-1) : undefined) ?? sourceRoutes.get(target) ?? `https://github.com/MidgardAI/vibeke/blob/main/${relative(repoRoot, target).split(sep).join('/')}`
      return `](${route}${hash ? '#' + hash : ''}${title})`
    })
  return body.replace(/\{\{#include\s+([^}]+)\}\}/g, (_, include: string) =>
    expandMarkdown(resolve(dirname(file), include.trim()), [...stack, file]),
  )
}

export function loadDocs(): Doc[] {
  return docManifest.map(doc => {
    // The page template owns the title; included chapters can also start with an H1.
    const body = expandMarkdown(resolve(bookRoot, doc.file)).replace(/^# .+\n/, '').replace(/^# /gm, '## ').trim()
    const slugger = new GithubSlugger()
    let fence = false
    const headings: Doc['headings'] = []
    for (const line of body.split('\n')) {
      if (/^\s*(```|~~~)/.test(line)) fence = !fence
      if (fence) continue
      const match = /^(#{2,3}) (.+)$/.exec(line)
      if (match) {
        const text = match[2]!.replace(/[`*_]/g, '').replace(/\[([^\]]+)\]\([^)]+\)/g, '$1')
        headings.push({ id: slugger.slug(text), text, depth: match[1]!.length })
      }
    }
    return { ...doc, body, headings }
  })
}

export function loadApiMethods() {
  return JSON.parse(readFileSync(resolve(docsRoot, 'api/methods.json'), 'utf8')).methods.map(({ name, params, result, mutating, pane_scope }: { name: string; params: string; result: string; mutating: boolean; pane_scope: string }) => ({ name, params, result, mutating, pane_scope }))
}

export function docsPlugin(): Plugin {
  return {
    name: 'vibeke-docs',
    resolveId(id) { if (id === virtualId) return resolvedId },
    load(id) {
      if (id === resolvedId) return `export const docs = ${JSON.stringify(loadDocs())}; export const apiMethods = ${JSON.stringify(loadApiMethods())}`
    },
    generateBundle() {
      for (const file of apiFiles) this.emitFile({ type: 'asset', fileName: `api-reference/${file}`, source: readFileSync(resolve(docsRoot, 'api', file)) })
    },
    configureServer(server) {
      server.watcher.add(docsRoot)
      server.middlewares.use((req, res, next) => {
        const file = req.url?.replace('/api-reference/', '')
        if (!req.url?.startsWith('/api-reference/') || !apiFiles.includes(file!)) return next()
        res.setHeader('Content-Type', 'application/json')
        res.end(readFileSync(resolve(docsRoot, 'api', file!)))
      })
    },
    handleHotUpdate({ file, server }) {
      if (!file.startsWith(docsRoot + sep) || !(/\.(md|json)$/).test(file)) return
      const module = server.moduleGraph.getModuleById(resolvedId)
      if (module) server.moduleGraph.invalidateModule(module)
      server.ws.send({ type: 'full-reload' })
    },
  }
}
