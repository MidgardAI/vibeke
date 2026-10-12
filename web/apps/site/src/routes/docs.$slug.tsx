import { createFileRoute, notFound } from '@tanstack/react-router'
import { docs } from 'virtual:vibeke-docs'
import { notFoundHead, pageHead } from '../lib/seo'
import { DocArticle } from '../components/docs'

export const Route = createFileRoute('/docs/$slug')({
  loader: ({ params }) => {
    const doc = docs.find(doc => doc.slug === params.slug)
    if (!doc) throw notFound()
    return doc
  },
  head: ({ loaderData }) => loaderData
    ? pageHead({ path: `/docs/${loaderData.slug}`, title: `${loaderData.title} — Vibeke Docs`, description: loaderData.description, doc: true })
    : notFoundHead(),
  component: () => <DocArticle doc={Route.useLoaderData()} />,
})
