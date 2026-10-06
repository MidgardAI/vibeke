// A created invitation link (share or handoff, spec 16 §15): the link with Copy, the OS share
// sheet when available, and a QR code for a phone next to you.

import { Copy, Share2 } from 'lucide-react';
import { useApp, useNow } from '../app/hooks';
import { t } from '../i18n';
import { whenText } from '../lib/format';
import { QrCode } from './qr-code';
import { Button, Notice } from './ui';

export interface Invite {
  link: string;
  /** Unix seconds the link must be opened by. */
  openBy?: number;
}

export function InviteLink({ invite, shareTitle, note }: { invite: Invite; shareTitle: string; note?: string }) {
  const app = useApp();
  const now = useNow(15_000);
  const share = app.platform.share;
  const expired = invite.openBy !== undefined && invite.openBy * 1000 <= now;
  return (
    <div className="space-y-3">
      <div className="mx-auto w-full max-w-[16rem]">
        <QrCode value={invite.link} label={t.share.link} className={expired ? 'opacity-30' : undefined} />
      </div>
      <div className="break-all rounded-xl border border-border bg-bg px-3 py-2 font-mono text-xs select-all">{invite.link}</div>
      {invite.openBy !== undefined && (
        <div className={expired ? 'text-xs text-danger' : 'text-xs text-muted'}>{t.share.openBy(whenText(invite.openBy * 1000, now))}</div>
      )}
      <div className="flex gap-2">
        <Button
          className="flex-1"
          variant="primary"
          icon={<Copy className="size-4" />}
          disabled={expired}
          onClick={() =>
            void app.platform.clipboard.writeText(invite.link).then(
              () => app.toast(t.copied, 'ok'),
              (e: unknown) => app.toast((e as Error).message, 'error'),
            )
          }
        >
          {t.share.copyLink}
        </Button>
        {share && (
          <Button
            className="flex-1"
            variant="outline"
            icon={<Share2 className="size-4" />}
            disabled={expired}
            onClick={() => void share({ title: shareTitle, url: invite.link }).catch(() => {})}
          >
            {t.share.shareLink}
          </Button>
        )}
      </div>
      {note && <Notice>{note}</Notice>}
    </div>
  );
}
