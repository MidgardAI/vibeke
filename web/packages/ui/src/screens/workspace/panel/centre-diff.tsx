// The transient centre view for a diff (`?view=diff&file=…`, from ⌥-click in the Changes tab):
// the same diff as the panel's inline one, with room to read. The workspace screen renders it in
// place of the tab's content while the route asks for it; closing drops `view` and `file`.

import { useMemo } from 'react';
import { selectedPane, useWorkspaceRows } from '../../../app/selection';
import { buildFileTree, fileOrder } from '../../../lib/file-tree';
import { navigate, type WorkspaceRoute } from '../../../router';
import { useChangeFiles } from './changes-data';
import { DiffPane } from './diff-pane';
import { diffSource } from './routes';

export default function CentreDiff({ route }: { route: WorkspaceRoute }) {
  const rows = useWorkspaceRows();
  const row = rows.find((r) => r.host === route.host && r.workspace.id === route.workspace);
  const pane = selectedPane(route, row);
  const src = diffSource(route);
  const data = useChangeFiles(route.host, pane, src, { poll: false });
  const ordered = useMemo(() => {
    const by = new Map(data.files.map((f) => [f.path, f]));
    return fileOrder(buildFileTree(data.files))
      .map((p) => by.get(p)!)
      .filter(Boolean);
  }, [data.files]);
  if (!pane || !route.file) return null;
  return (
    <DiffPane
      centre
      host={route.host}
      pane={pane}
      src={src}
      path={route.file}
      files={ordered}
      reload={data.signature}
      onPath={(p) => navigate({ ...route, file: p }, { replace: true })}
      onClose={() => navigate({ ...route, file: null, view: null }, { replace: true })}
    />
  );
}
