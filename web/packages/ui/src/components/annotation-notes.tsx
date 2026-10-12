// The notes of a page review: a numbered list with a text box per note, a note for the whole page,
// and Send / Copy / Clear. Shared by the desktop preview (a column beside the page) and the phone
// (a bottom sheet). Thumbnails exist only for picks this device captured; notes picked on the host
// show text only, because the screenshot is a file on the host.

import { useEffect, useState } from 'react';
import { Check, Clipboard, Send, Trash2, X } from 'lucide-react';
import type { AnnotationDraft, AnnotationNote } from '@vibeke/core';
import { t } from '../i18n';
import { isEditable, noteTitle } from '../lib/annotations';
import { Button, IconButton, Pill, cx } from './ui';

export interface AnnotationNotesProps {
  draft: AnnotationDraft | null;
  /** Screenshots captured on this device, by note number (data URLs). */
  thumbs?: ReadonlyMap<number, string>;
  busy?: boolean;
  onEdit(n: number, note: string): void;
  onPageNote(note: string): void;
  onRemove(n: number): void;
  onSend(): void;
  onCopy(): void;
  onClear(): void;
  className?: string;
}

export function AnnotationNotes({ draft, thumbs, busy, onEdit, onPageNote, onRemove, onSend, onCopy, onClear, className }: AnnotationNotesProps) {
  const s = t.annotate;
  const notes = draft?.notes.filter((n) => n.kind !== 'page') ?? [];
  const editable = isEditable(draft);
  const hasContent = notes.length > 0 || !!draft?.page_note;
  return (
    <section aria-label={s.notesLabel} className={cx('flex min-h-0 flex-col gap-3', className)}>
      <div className="flex items-center gap-2">
        <h3 className="min-w-0 flex-1 truncate text-sm font-semibold">{s.title}</h3>
        {draft && <Pill>{s.state[draft.state]}</Pill>}
      </div>
      <div className="vk-scroll flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto">
        {notes.length === 0 && <p className="text-sm text-muted">{s.empty}</p>}
        {notes.map((n) => (
          <NoteCard key={n.n} note={n} thumb={thumbs?.get(n.n)} editable={editable} onEdit={onEdit} onRemove={onRemove} />
        ))}
        {draft && <PageNote key={draft.id} value={draft.page_note ?? ''} disabled={!editable} onCommit={onPageNote} />}
      </div>
      <div className="flex flex-wrap gap-2">
        <Button size="sm" variant="primary" icon={<Send />} disabled={busy || !editable || !hasContent} onClick={onSend}>
          {s.send}
        </Button>
        <Button size="sm" variant="outline" icon={<Clipboard />} disabled={busy || !draft || !hasContent} onClick={onCopy}>
          {s.copy}
        </Button>
        <Button size="sm" variant="ghost" icon={<Trash2 />} disabled={busy || !draft} onClick={onClear}>
          {s.clear}
        </Button>
      </div>
    </section>
  );
}

const area =
  'vk-focus w-full resize-none rounded-md border border-border bg-bg px-2 py-1.5 text-sm text-fg placeholder:text-faint pointer-coarse:text-base disabled:opacity-60';

function NoteCard({ note, thumb, editable, onEdit, onRemove }: { note: AnnotationNote; thumb?: string; editable: boolean; onEdit(n: number, note: string): void; onRemove(n: number): void }) {
  const s = t.annotate;
  const [text, setText] = useState(note.note);
  // The host's copy changed (an edit from another surface): follow it unless this box is being typed in.
  const [focused, setFocused] = useState(false);
  useEffect(() => {
    if (!focused) setText(note.note);
  }, [note.note, focused]);
  const commit = () => {
    setFocused(false);
    if (text !== note.note) onEdit(note.n, text);
  };
  return (
    <div className="rounded-lg border border-border bg-surface p-2">
      <div className="mb-1.5 flex items-start gap-2">
        <span aria-hidden className="mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-full bg-accent text-xs font-semibold text-accent-fg">
          {note.n}
        </span>
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium" title={note.selector}>
            {noteTitle(note)}
          </div>
          <div className="truncate font-mono text-xs text-muted" title={note.selector}>
            {note.role ? `${note.role} · ` : ''}
            {note.selector || s.kind[note.kind]}
          </div>
          {note.kind === 'region' && note.covers && note.covers.length > 0 && <div className="text-xs text-muted">{s.covers(note.covers.length)}</div>}
        </div>
        {editable && (
          <IconButton label={s.removeNote(note.n)} onClick={() => onRemove(note.n)}>
            <X />
          </IconButton>
        )}
      </div>
      {thumb && <img src={thumb} alt={s.screenshot} className="mb-1.5 max-h-28 w-full rounded border border-border object-contain object-left bg-white" />}
      <textarea
        aria-label={s.noteLabel(note.n)}
        placeholder={s.notePlaceholder}
        rows={2}
        value={text}
        disabled={!editable}
        onFocus={() => setFocused(true)}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        className={area}
      />
    </div>
  );
}

function PageNote({ value, disabled, onCommit }: { value: string; disabled: boolean; onCommit(note: string): void }) {
  const [text, setText] = useState(value);
  const [focused, setFocused] = useState(false);
  useEffect(() => {
    if (!focused) setText(value);
  }, [value, focused]);
  return (
    <label className="block">
      <span className="mb-1 flex items-center gap-1 text-xs text-muted">
        <Check className="size-3" aria-hidden />
        {t.annotate.pageNote}
      </span>
      <textarea
        placeholder={t.annotate.pageNotePlaceholder}
        rows={2}
        value={text}
        disabled={disabled}
        onFocus={() => setFocused(true)}
        onChange={(e) => setText(e.target.value)}
        onBlur={() => {
          setFocused(false);
          if (text !== value) onCommit(text);
        }}
        className={area}
      />
    </label>
  );
}
