import { createFileRoute, notFound } from '@tanstack/react-router'
import { docs } from 'virtual:vibeke-docs'
import { DocArticle } from '../components/docs'

export const Route = createFileRoute('/docs/$slug')({
  loader: ({ params }) => {
    const doc = docs.find(doc => doc.slug === params.slug)
    if (!doc) throw notFound()
    return doc
  },
  head: ({ loaderData }) => ({ meta: [{ title: loaderData ? `${loaderData.title} — Vibeke Docs` : 'Page not found — Vibeke' }, { name: 'description', content: loaderData?.description ?? 'This documentation page could not be found.' }] }),
  component: () => <DocArticle doc={Route.useLoaderData()} />,
})
