import type { Doc } from '../../content/manifest'

export function searchDocs(docs: Doc[], query: string) {
  const terms = query.toLowerCase().trim().split(/\s+/).filter(Boolean)
  if (!terms.length) return docs.filter(doc => ['quickstart', 'install', 'agents', 'cli', 'config'].includes(doc.slug))
  return docs.map(doc => {
    const title = doc.title.toLowerCase()
    const summary = `${doc.description} ${doc.group}`.toLowerCase()
    const body = doc.body.toLowerCase()
    const score = terms.every(term => `${title} ${summary} ${body}`.includes(term))
      ? terms.reduce((total, term) => total + (title.includes(term) ? 10 : summary.includes(term) ? 5 : 1), 0) : 0
    return { doc, score }
  }).filter(result => result.score > 0).sort((a, b) => b.score - a.score).slice(0, 10).map(result => result.doc)
}
