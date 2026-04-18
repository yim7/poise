import { useState } from 'react';

import { AppShell } from '@/app/AppShell';
import { WorkbenchStoreProvider, createWorkbenchStore } from '@/state/workbenchStore';
import {
  createBrowserSessionPersistence,
  createSessionSync,
} from '@/state/sessionSync';

export function App() {
  const [store] = useState(() =>
    createWorkbenchStore({
      sessionSync: createSessionSync(createBrowserSessionPersistence(window.localStorage)),
    }),
  );

  return (
    <WorkbenchStoreProvider store={store}>
      <AppShell />
    </WorkbenchStoreProvider>
  );
}
