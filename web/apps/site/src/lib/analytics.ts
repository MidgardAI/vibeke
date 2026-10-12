import posthog from 'posthog-js'

// Intent events, not completed stars or installations. Never send copied code,
// page content, arbitrary URLs, or query strings. Existing autocapture is separate.
// Both PostHog and the existing Google Ads page tag are disabled for browser tests.
export type SiteEvent = 'github_outbound' | 'install_intent' | 'install_copy' | 'connection_intent' | 'demo_start' | 'demo_progress'
type Properties = { placement: string; target?: string; chapter?: string; quartile?: number }
export function trackSiteEvent(event: SiteEvent, properties: Properties) {
  if (typeof navigator === 'undefined' || navigator.webdriver) return
  try { posthog.capture(event, properties) } catch { /* Analytics must not interrupt navigation or playback. */ }
}
