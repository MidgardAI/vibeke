// The workspace's right panel: Files / Changes tabs over the selected pane's repository.
// Lazy-loaded by the layout (app/layout.tsx); the Files viewer loads on first use.

import { FileDiff, FolderTree, Images, PanelRightClose, X } from 'lucide-react';
import { useApp } from '../../../app/hooks';
import { isMacLike } from '../../../app/keyboard';
import { useWorkspaceShots } from '../../../app/screenshot-store';
import { useTogglePanel } from '../../../app/layout';
import { selectedPane, useWorkspaceRows } from '../../../app/selection';
import { Badge, Empty, IconButton, Tabs } from '../../../components/ui';
import { t } from '../../../i18n';
import { keyLabel } from '../../../lib/shortcuts';
import type { PanelKind, WorkspaceRoute } from '../../../router';
import { ChangesTab } from './changes-tab';
import { FilesTab } from './files-tab';
import { ScreenshotsTab } from './screenshots-tab';

export default function RightPanel({ route, kind, sheet = false }: { route: WorkspaceRoute; kind: PanelKind; sheet?: boolean }) {
  const app = useApp();
  const rows = useWorkspaceRows();
  const toggle = useTogglePanel();
  const mac = isMacLike(app.platform.mac);
  const row = rows.find((r) => r.host === route.host && r.workspace.id === route.workspace);
  const pane = selectedPane(route, row);
  const unread = useWorkspaceShots(route.host, route.workspace).unread.size;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="titlebar flex h-11 shrink-0 items-center gap-1 border-b border-border px-2">
        <Tabs
          label={t.workspace.panel}
          value={kind}
          onChange={(k) => k !== kind && toggle(route, k)}
          items={[
            { value: 'files', label: t.workspace.files, icon: <FolderTree /> },
            { value: 'changes', label: t.workspace.changes, icon: <FileDiff /> },
            { value: 'screenshots', label: t.workspace.screenshots, icon: <Images />, badge: kind === 'screenshots' ? undefined : <Badge n={unread} /> },
          ]}
        />
        <span className="flex-1" />
        <IconButton label={sheet ? t.workspace.closePanel : `${t.workspace.closePanel} (${keyLabel(mac, 'mod+3')})`} onClick={() => toggle(route, kind)}>
          {sheet ? <X /> : <PanelRightClose />}
        </IconButton>
      </div>
      {kind === 'screenshots' ? (
        <ScreenshotsTab route={route} />
      ) : !pane ? (
        <Empty icon={kind === 'files' ? <FolderTree /> : <FileDiff />} title={t.changes.noPane} />
      ) : kind === 'files' ? (
        <FilesTab key={`${route.host}/${pane}`} route={route} pane={pane} />
      ) : (
        <ChangesTab key={`${route.host}/${pane}`} route={route} pane={pane} />
      )}
    </div>
  );
}
