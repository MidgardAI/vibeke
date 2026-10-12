import { expect, test } from 'bun:test'
import { canonicalUrl, jsonLd } from '../src/lib/seo'

test('canonical URLs exclude queries, fragments and trailing slashes', () => {
  expect(canonicalUrl('/')).toBe('https://vibeke.dev/')
  expect(canonicalUrl('/docs/')).toBe('https://vibeke.dev/docs')
  expect(canonicalUrl('/docs/install/?utm_source=share#linux')).toBe('https://vibeke.dev/docs/install')
})

test('structured data cannot close its script element', () => {
  const value = { name: '</script><script>alert(1)</script>' }
  const script = jsonLd(value)
  expect(script.children).not.toContain('<')
  expect(JSON.parse(script.children)).toEqual(value)
})
