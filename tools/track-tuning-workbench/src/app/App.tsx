import { useState } from 'react';

import { AppShell } from '@/app/AppShell';
import {
  createBridgeSessionPersistence,
  createWorkbenchBridge,
} from '@/app/workbenchBridge';
import { WorkbenchStoreProvider, createWorkbenchStore } from '@/state/workbenchStore';
import {
  createBrowserSessionPersistence,
  createSessionSync,
} from '@/state/sessionSync';

export function App() {
  const [{ bridge, store }] = useState(() => {
    const nextBridge = createWorkbenchBridge();
    const persistence = nextBridge.isTauriEnvironment()
      ? createBridgeSessionPersistence(nextBridge, window.localStorage)
      : createBrowserSessionPersistence(window.localStorage);

    return {
      bridge: nextBridge,
      store: createWorkbenchStore({
        sessionSync: createSessionSync(persistence, { debounceMs: 0 }),
      }),
    };
  });

  return (
    <WorkbenchStoreProvider store={store}>
      <AppShell bridge={bridge} />
    </WorkbenchStoreProvider>
  );
}
