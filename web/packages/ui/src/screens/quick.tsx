// The menu-bar popover (spec 16 §16.2): the compact inbox, then every agent on every connected
// host. Same cards, batches and keyboard as the Inbox (j/k also walk the agents); Esc closes;
// "Approve…" from a notification lands here with a confirm.

import { useEffect, useState } from 'react';
import { AppWindow, Check } from 'lucide-react';
import { interactionRisk, type InboxItem } from '@vibeke/core';
import { useApp, useInboxItems, useTree } from '../app/hooks';
import { ConnectionBanner, Toasts } from '../app/shell';
import { CardHeader } from '../components/interaction-card';
import { Button, IconButton, RiskBadge, Sheet } from '../components/ui';
import { t } from '../i18n';
import { listNav } from '../lib/list-nav';
import { formatRoute, navigate, useRoute } from '../router';
import { InboxScreen } from './inbox';
import { QuickAgents } from './quick-agents';

export function QuickScreen() {
  const app = useApp();
  const items = useInboxItems();
  const tree = useTree();
  const route = useRoute();
  const [confirm, setConfirm] = useState<InboxItem | null>(null);
  const [gone, setGone] = useState(false);

  // `#/i/<host>/<id>[?do=allow]` (notification "Approve…" / click): select that card, confirm.
  useEffect(() => {
    if (route.name === 'inbox' || route.name === 'home') return;
    if (route.name === 'interaction') {
      const it = items.find((x) => x.host_id === route.host && x.interaction.id === route.id);
      navigate({ name: 'inbox' }, { replace: true });
      if (!it) {
        setGone(true);
        return;
      }
      setGone(false);
      requestAnimationFrame(() => listNav.selectKey(`${route.host}/${route.id}`));
      if (route.preselect === 'allow' && it.interaction.kind === 'approval' && it.interaction.answerable) setConfirm(it);
      return;
    }
    // Anything else (a pane, settings…) belongs in the main window.
    app.platform.windows?.openMain?.(formatRoute(route));
    navigate({ name: 'inbox' }, { replace: true });
    app.platform.windows?.close?.();
  }, [route, items, app]);

  const working = tree.all.filter((r) => r.attention === 'working').length;
  const it = confirm?.interaction;
  const risky = it ? interactionRisk(it) === 'high' || interactionRisk(it) === 'unknown' : false;
  const what = it ? (it.action?.command ? `\`${it.action.command}\`` : it.title) : '';

  return (
    <div className="quick-root flex h-full flex-col">
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-3">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-semibold leading-tight">{t.quick.title}</div>
          <div className="text-xs text-muted" aria-live="polite">
            {t.quick.needYou(tree.needYou.length)}
            {working > 0 && ` · ${t.quick.working(working)}`}
          </div>
        </div>
        <IconButton label={t.quick.openApp} onClick={() => (app.platform.windows?.openMain?.('#/inbox'), app.platform.windows?.close?.())}>
          <AppWindow className="size-5" />
        </IconButton>
      </header>
      <ConnectionBanner />
      <main className="min-h-0 flex-1 overflow-y-auto">
        {gone && <div className="px-4 pt-3 text-sm text-muted">{t.quick.gone}</div>}
        {/* The agents share the inbox's keyboard list: j/k walk the cards, then the agents. */}
        <InboxScreen after={<QuickAgents tree={tree} />} />
      </main>
      <footer className="shrink-0 border-t border-border px-3 py-1.5 text-center text-2xs text-faint">j / k · a {t.inbox.allow.toLowerCase()} · d {t.inbox.deny.toLowerCase()} · ↵ {t.open.toLowerCase()} · esc</footer>
      <Toasts />
      <Sheet open={!!confirm} onClose={() => setConfirm(null)} title={t.quick.confirmTitle} role="alertdialog">
        {confirm && it && (
          <div className="space-y-3">
            <CardHeader item={confirm} showHost />
            <div className="flex items-center gap-2">
              <RiskBadge risk={interactionRisk(it)} />
            </div>
            <p className="text-sm">{risky ? t.inbox.confirmHigh(what) : t.quick.confirmBody(what)}</p>
            {it.action?.command && <pre className="term max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-xl border border-border p-2 text-xs">{it.action.command}</pre>}
            <div className="flex gap-2">
              <Button className="flex-1" variant="outline" onClick={() => setConfirm(null)}>
                {t.cancel}
              </Button>
              <Button
                className="flex-1"
                variant={risky ? 'danger' : 'ok'}
                icon={<Check className="size-4" />}
                data-autofocus
                onClick={() => {
                  const c = confirm;
                  setConfirm(null);
                  app.haptic('tap');
                  void app.answer(c.host_id, c.interaction, { decision: 'allow' }, 'allow');
                }}
              >
                {risky ? t.inbox.confirmAllow : t.quick.approve}
              </Button>
            </div>
          </div>
        )}
      </Sheet>
    </div>
  );
}
