// Body of a picker card: the agent's own menu shown natively. A single-select list answers on tap,
// a multi-select collects checkboxes and confirms the full set, a left/right adjuster (effort and
// the like) is a segmented control, and Cancel sends the picker's cancel key. A dialog the host
// could not parse (`picker.name === "unknown"`) offers Cancel and the terminal only.

import { useRef, useState, type KeyboardEvent } from 'react';
import { Check, Loader2, SquareTerminal, X } from 'lucide-react';
import type { Interaction } from '@vibeke/core';
import { t } from '../i18n';
import { adjustAnswer, cancelAnswer, canCancel, chooseAnswer, confirmAnswer, initialChecked, isUnknownDialog, pickerQuestion, toggleId, type PickerAnswer } from '../lib/pickers';
import { Button, Segmented, cx } from './ui';

/** Move focus along a group of buttons with the arrow keys (radio / list semantics). */
function roving(e: KeyboardEvent<HTMLElement>, keys: readonly [string, string]): void {
  const dir = e.key === keys[0] ? -1 : e.key === keys[1] ? 1 : 0;
  if (!dir) return;
  const items = [...e.currentTarget.querySelectorAll<HTMLButtonElement>('button:not(:disabled)[role=radio],button:not(:disabled)[role=checkbox]')];
  if (!items.length) return;
  const at = items.indexOf(document.activeElement as HTMLButtonElement);
  const next = items[(at < 0 ? (dir > 0 ? 0 : items.length - 1) : (at + dir + items.length) % items.length)]!;
  e.preventDefault();
  next.focus();
}

export function PickerBody({
  it,
  disabled,
  locked,
  onAnswer,
  onOpenTerminal,
}: {
  it: Interaction;
  disabled: boolean;
  /** An answer is in flight or delivered: nothing more to tap. */
  locked: boolean;
  onAnswer(a: PickerAnswer, label: string): void;
  onOpenTerminal(): void;
}) {
  // Escape inside the card dismisses the dialog (desktop), as it does in the agent itself.
  const onKeyDown = (e: KeyboardEvent<HTMLElement>) => {
    if (e.key === 'Escape' && canCancel(it) && !disabled && !locked) {
      e.preventDefault();
      onAnswer(cancelAnswer(it), 'cancel');
    }
  };
  return (
    <div onKeyDown={onKeyDown}>
      {isUnknownDialog(it) ? (
        <UnknownBody it={it} disabled={disabled} locked={locked} onAnswer={onAnswer} onOpenTerminal={onOpenTerminal} />
      ) : (
        <ListBody key={it.picker?.signature ?? it.id} it={it} disabled={disabled} locked={locked} onAnswer={onAnswer} />
      )}
    </div>
  );
}

function CancelButton({ it, disabled, onAnswer }: { it: Interaction; disabled: boolean; onAnswer(a: PickerAnswer, label: string): void }) {
  if (!canCancel(it)) return null;
  return (
    <Button variant="outline" size="sm" disabled={disabled} icon={<X className="size-4" />} data-act="cancel" aria-keyshortcuts="Escape" onClick={() => onAnswer(cancelAnswer(it), 'cancel')}>
      {t.picker.cancel}
    </Button>
  );
}

function UnknownBody({ it, disabled, locked, onAnswer, onOpenTerminal }: { it: Interaction; disabled: boolean; locked: boolean; onAnswer(a: PickerAnswer, label: string): void; onOpenTerminal(): void }) {
  return (
    <div className="space-y-2.5" data-picker="unknown">
      <p className="text-sm text-muted">{t.picker.unknownBody}</p>
      {!canCancel(it) && <p className="text-xs text-faint">{t.picker.noCancel}</p>}
      {!locked && (
        <div className="flex flex-wrap gap-2">
          <CancelButton it={it} disabled={disabled} onAnswer={onAnswer} />
          <Button variant="primary" size="sm" icon={<SquareTerminal className="size-4" />} data-act="open-terminal" onClick={onOpenTerminal}>
            {t.picker.openTerminal}
          </Button>
        </div>
      )}
    </div>
  );
}

function ListBody({ it, disabled, locked, onAnswer }: { it: Interaction; disabled: boolean; locked: boolean; onAnswer(a: PickerAnswer, label: string): void }) {
  const q = pickerQuestion(it);
  const multi = !!q?.multi;
  const [checked, setChecked] = useState<string[]>(() => (q ? initialChecked(q) : []));
  const [tapped, setTapped] = useState<string | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const adj = it.picker?.left_right;
  const options = q?.options ?? [];

  const choose = (id: string) => {
    setTapped(id);
    onAnswer(chooseAnswer(it, id), id);
  };

  return (
    <div className="space-y-2.5" data-picker={it.picker?.name ?? 'menu'}>
      {it.body_md && <p className="text-sm text-muted">{it.body_md}</p>}
      {multi && <div className="text-2xs text-faint">{t.picker.multiHint}</div>}
      {options.length > 0 && (
        <div
          ref={listRef}
          role={multi ? 'group' : 'radiogroup'}
          aria-label={q?.prompt || it.title}
          className="flex flex-col gap-1.5"
          onKeyDown={(e) => roving(e, ['ArrowUp', 'ArrowDown'])}
        >
          {options.map((o) => {
            const on = multi ? checked.includes(o.id) : !!o.selected;
            const busy = tapped === o.id && locked;
            return (
              <button
                key={o.id}
                type="button"
                data-pick={o.id}
                role={multi ? 'checkbox' : 'radio'}
                aria-checked={on}
                disabled={disabled}
                onClick={() => (multi ? setChecked((c) => toggleId(options, c, o.id)) : choose(o.id))}
                className={cx(
                  'vk-focus flex items-center gap-2.5 rounded-xl border px-3 py-2 text-left text-sm disabled:opacity-50',
                  on ? 'border-accent bg-accent/10' : 'border-border bg-bg',
                )}
              >
                <span
                  aria-hidden
                  className={cx(
                    'inline-flex size-4 shrink-0 items-center justify-center border',
                    multi ? 'rounded-[4px]' : 'rounded-full',
                    on ? 'border-accent bg-accent text-accent-fg' : 'border-border-strong',
                  )}
                >
                  {on && <Check className="size-3" strokeWidth={3} />}
                </span>
                <span className="min-w-0 flex-1">
                  <span className="block font-medium">{o.label}</span>
                  {o.description && <span className="block text-xs text-muted">{o.description}</span>}
                </span>
                {busy && <Loader2 className="size-4 shrink-0 animate-spin text-muted" aria-hidden />}
                {!multi && on && !busy && <span className="shrink-0 text-2xs font-medium text-accent">{t.picker.current}</span>}
              </button>
            );
          })}
        </div>
      )}
      {adj && adj.values.length > 0 && (
        <div className="space-y-1" onKeyDown={(e) => roving(e, ['ArrowLeft', 'ArrowRight'])} data-adjust>
          <div className="text-2xs font-semibold uppercase tracking-wide text-faint">{t.picker.adjustLabel(adj.verb)}</div>
          <div className={cx(disabled && 'pointer-events-none opacity-50')}>
            <Segmented
              label={adj.verb}
              value={adj.current ?? ''}
              options={adj.values.map((v) => ({ value: v, label: v }))}
              onChange={(v) => onAnswer(adjustAnswer(it, v), v)}
            />
          </div>
        </div>
      )}
      {!locked && (
        <div className="flex flex-wrap items-center gap-2">
          {multi && (
            <Button variant="primary" size="sm" className="flex-1" disabled={disabled} icon={<Check className="size-4" />} data-act="confirm" onClick={() => onAnswer(confirmAnswer(it, checked), 'confirm')}>
              {t.picker.confirm}
            </Button>
          )}
          <CancelButton it={it} disabled={disabled} onAnswer={onAnswer} />
        </div>
      )}
    </div>
  );
}
