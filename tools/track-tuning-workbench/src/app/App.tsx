import { useState } from 'react';

import { AppShell } from '@/app/AppShell';
import { WorkbenchStoreProvider, createWorkbenchStore } from '@/state/workbenchStore';

export function App() {
  const [store] = useState(() => createWorkbenchStore());

  return (
    <WorkbenchStoreProvider store={store}>
      <AppShell />
    </WorkbenchStoreProvider>
  );
}
