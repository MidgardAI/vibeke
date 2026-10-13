// `#/attest?t=…|error=…`: the account server's return from a GitHub sign-in (spec 20 §2.3). The
// result is stored for the pending invitation, then the pairing or review screen resumes.

import { useEffect, useState } from 'react';
import { useApp } from '../app/hooks';
import { Notice, Spinner } from '../components/ui';
import { t } from '../i18n';
import { finishAttestation } from '../lib/attest-flow';
import { navigate } from '../router';

export function AttestScreen({ query }: { query: string }) {
  const app = useApp();
  const [lost, setLost] = useState(false);
  useEffect(() => {
    // Read once: the token must not stay in the address bar or the history.
    const next = finishAttestation(app, query);
    if (next) navigate(next, { replace: true });
    else {
      navigate({ name: 'attest', query: '' }, { replace: true });
      setLost(true);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return (
    <div className="flex h-full items-center justify-center p-4">
      {lost ? <Notice>{t.attest.nothingPending}</Notice> : <Spinner />}
    </div>
  );
}
