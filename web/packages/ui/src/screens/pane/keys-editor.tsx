// Editor sheet for the keys board: pick a layout, add, move and remove keys, build your own
// chords and sequences, and share the layout as a short code.

import { useState, type ReactNode } from 'react';
import { ArrowDown, ArrowUp, Copy, Plus, Trash2, X } from 'lucide-react';
import { useApp, usePrefs } from '../../app/hooks';
import { Button, Segmented, Sheet, Toggle, cx } from '../../components/ui';
import { t } from '../../i18n';
import {
  DEFAULT_LAYOUT, MAX_KEYS, MAX_LABEL, MAX_STEPS, PRESET_IDS, addKey, buildStep, decodeLayout, encodeLayout, isDangerous, moveKey, padLabel, preset, removeKey, setSpan,
  type KeyLayout, type Span,
} from '../../lib/key-layout';

const MODS = ['ctrl', 'alt', 'shift'] as const;

export function KeysEditor({ open, onClose }: { open: boolean; onClose(): void }) {
  const app = useApp();
  const prefs = usePrefs();
  const layout: KeyLayout = prefs.keyLayout ?? DEFAULT_LAYOUT;
  const save = (keys: KeyLayout['keys'], name = layout.name) => app.prefs.patch({ keyLayout: { name, keys } });

  const [label, setLabel] = useState('');
  const [mods, setMods] = useState<Partial<Record<(typeof MODS)[number], boolean>>>({});
  const [keyName, setKeyName] = useState('');
  const [steps, setSteps] = useState<string[]>([]);
  const [span, setNewSpan] = useState<Span>(1);
  const [builderError, setBuilderError] = useState<string | null>(null);
  const [code, setCode] = useState('');
  const [importText, setImportText] = useState('');
  const [importError, setImportError] = useState<string | null>(null);

  const pushStep = () => {
    if (steps.length >= MAX_STEPS) return setBuilderError(t.keysEditor.tooManySteps);
    const s = buildStep(mods, keyName);
    if (!s) return setBuilderError(t.keysEditor.badKey);
    setSteps([...steps, s]);
    setKeyName('');
    setMods({});
    setBuilderError(null);
  };

  const commit = () => {
    if (layout.keys.length >= MAX_KEYS) return setBuilderError(t.keysEditor.boardFull);
    // A filled-in but unadded step counts as the last step.
    const pending = keyName.trim() ? buildStep(mods, keyName) : null;
    if (keyName.trim() && !pending) return setBuilderError(t.keysEditor.badKey);
    const all = pending ? [...steps, pending] : steps;
    if (all.length === 0) return setBuilderError(t.keysEditor.noSteps);
    if (all.length > MAX_STEPS) return setBuilderError(t.keysEditor.tooManySteps);
    save(addKey(layout.keys, { steps: all, label, span }));
    setSteps([]);
    setLabel('');
    setKeyName('');
    setMods({});
    setBuilderError(null);
  };

  const doImport = () => {
    const r = decodeLayout(importText);
    if (!r.ok) return setImportError(t.keysEditor.errors[r.error] ?? t.keysEditor.errors.invalid!);
    app.prefs.patch({ keyLayout: r.layout });
    setImportText('');
    setImportError(null);
    app.toast(t.keysEditor.imported);
  };

  const copy = async () => {
    const c = encodeLayout(layout);
    setCode(c);
    try {
      await navigator.clipboard.writeText(c);
      app.toast(t.keysEditor.copied);
    } catch {
      // The code stays visible in the box to copy by hand.
    }
  };

  return (
    <Sheet open={open} onClose={onClose} title={t.keysEditor.title}>
      <div className="space-y-4">
        <section className="space-y-1.5">
          <h3 className="text-xs font-semibold text-muted">{t.keysEditor.presets}</h3>
          <div className="flex flex-wrap gap-1.5">
            {PRESET_IDS.map((id) => (
              <Button key={id} size="sm" variant="outline" onClick={() => app.prefs.patch({ keyLayout: id === 'default' ? null : preset(id) })}>
                {t.keysEditor.presetNames[id]}
              </Button>
            ))}
          </div>
        </section>

        <section className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <div className="text-sm">{t.keysEditor.leftHand}</div>
            <div className="text-xs text-muted">{t.keysEditor.leftHandHint}</div>
          </div>
          <Toggle label={t.keysEditor.leftHand} checked={prefs.leftHand} onChange={(v) => app.prefs.patch({ leftHand: v })} />
        </section>

        <section className="space-y-1.5">
          <h3 className="text-xs font-semibold text-muted">{t.keysEditor.yourKeys}</h3>
          {layout.keys.length === 0 && <p className="text-sm text-muted">{t.keysEditor.empty}</p>}
          <ul className="space-y-1">
            {layout.keys.map((x, i) => (
              <li key={`${i}-${x.steps.join(' ')}`} className="flex items-center gap-1.5 rounded-lg border border-border px-2 py-1">
                <span className="min-w-0 flex-1 truncate font-mono text-sm">
                  {padLabel(x)}
                  {isDangerous(x) && <span className="ml-1.5 font-sans text-2xs text-need">{t.keysEditor.dangerous}</span>}
                </span>
                <Segmented
                  label={t.keysEditor.width}
                  value={String(x.span) as '1' | '2' | '3'}
                  onChange={(v) => save(setSpan(layout.keys, i, Number(v) as Span))}
                  options={[{ value: '1', label: '1' }, { value: '2', label: '2' }, { value: '3', label: '3' }]}
                />
                <SmallIcon label={t.keysEditor.moveUp} disabled={i === 0} onClick={() => save(moveKey(layout.keys, i, i - 1))}>
                  <ArrowUp className="size-4" />
                </SmallIcon>
                <SmallIcon label={t.keysEditor.moveDown} disabled={i === layout.keys.length - 1} onClick={() => save(moveKey(layout.keys, i, i + 1))}>
                  <ArrowDown className="size-4" />
                </SmallIcon>
                <SmallIcon label={t.keysEditor.remove} onClick={() => save(removeKey(layout.keys, i))}>
                  <Trash2 className="size-4" />
                </SmallIcon>
              </li>
            ))}
          </ul>
        </section>

        <section className="space-y-2">
          <h3 className="text-xs font-semibold text-muted">{t.keysEditor.addTitle}</h3>
          <input aria-label={t.keysEditor.label} placeholder={t.keysEditor.label} value={label} maxLength={MAX_LABEL} onChange={(e) => setLabel(e.target.value)} className={FIELD} />
          <div className="text-xs text-muted">{t.keysEditor.steps}</div>
          <div className="flex min-h-8 flex-wrap items-center gap-1.5">
            {steps.map((s, i) => (
              <button key={i} type="button" aria-label={`${t.keysEditor.remove} ${s}`} onClick={() => setSteps(steps.filter((_, j) => j !== i))} className="inline-flex h-8 items-center gap-1 rounded-md bg-surface-2 px-2 font-mono text-xs">
                {s} <X className="size-3" />
              </button>
            ))}
          </div>
          <div className="flex items-center gap-1.5">
            {MODS.map((m) => (
              <button
                key={m}
                type="button"
                aria-pressed={!!mods[m]}
                onClick={() => setMods({ ...mods, [m]: !mods[m] })}
                className={cx('h-10 rounded-lg border px-2.5 text-sm', mods[m] ? 'border-accent bg-accent/15 text-accent' : 'border-border')}
              >
                {m}
              </button>
            ))}
            <input
              aria-label={t.keysEditor.keyName}
              placeholder={t.keysEditor.keyName}
              value={keyName}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              onChange={(e) => setKeyName(e.target.value)}
              className={cx(FIELD, 'min-w-0 flex-1 font-mono')}
            />
            <Button variant="outline" onClick={pushStep} className="h-10" icon={<Plus className="size-4" />}>
              {t.keysEditor.addStep}
            </Button>
          </div>
          <div className="text-2xs text-faint">{t.keysEditor.keyHint}</div>
          <div className="flex items-center justify-between gap-2">
            <Segmented
              label={t.keysEditor.width}
              value={String(span) as '1' | '2' | '3'}
              onChange={(v) => setNewSpan(Number(v) as Span)}
              options={[{ value: '1', label: '1' }, { value: '2', label: '2' }, { value: '3', label: '3' }]}
            />
            <Button variant="primary" onClick={commit}>
              {t.keysEditor.addToBoard}
            </Button>
          </div>
          {builderError && (
            <p role="alert" className="text-xs text-danger">
              {builderError}
            </p>
          )}
        </section>

        <section className="space-y-2">
          <h3 className="text-xs font-semibold text-muted">{t.keysEditor.shareTitle}</h3>
          <div className="flex gap-2">
            <Button variant="outline" size="sm" icon={<Copy className="size-4" />} onClick={() => void copy()}>
              {code ? t.keysEditor.copy : t.keysEditor.export}
            </Button>
          </div>
          {code && <textarea readOnly aria-label={t.keysEditor.export} value={code} rows={3} onFocus={(e) => e.currentTarget.select()} className={cx(FIELD, 'h-auto break-all py-2 font-mono text-xs')} />}
          <textarea
            aria-label={t.keysEditor.importLabel}
            placeholder={t.keysEditor.importLabel}
            value={importText}
            rows={2}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            onChange={(e) => {
              setImportText(e.target.value);
              setImportError(null);
            }}
            className={cx(FIELD, 'h-auto py-2 font-mono text-xs')}
          />
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" disabled={!importText.trim()} onClick={doImport}>
              {t.keysEditor.import}
            </Button>
            {importError && (
              <span role="alert" className="text-xs text-danger">
                {importError}
              </span>
            )}
          </div>
        </section>
      </div>
    </Sheet>
  );
}

// 16px text so iOS does not zoom on focus.
const FIELD = 'block h-10 w-full rounded-lg border border-border bg-bg px-3 text-[16px] sm:text-sm';

function SmallIcon({ label, onClick, disabled, children }: { label: string; onClick(): void; disabled?: boolean; children: ReactNode }) {
  return (
    <button type="button" aria-label={label} title={label} disabled={disabled} onClick={onClick} className="inline-flex size-9 shrink-0 items-center justify-center rounded-md text-muted active:bg-surface-2 disabled:opacity-30">
      {children}
    </button>
  );
}
