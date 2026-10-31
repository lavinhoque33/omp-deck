// The settings sections pushed over the Settings hub (`Screen { kind: 'settings', section }`).

import type { ReactNode } from 'react';
import type { SettingsSection } from '../../nav.ts';
import '../../../styles/settings.css';
import { DeviceSection } from './Device.tsx';
import { BackupSection } from './Backup.tsx';
import { NotificationsSection } from './Notifications.tsx';
import { OmpSettingsSection } from './OmpSettings.tsx';
import { SyncSection } from './Sync.tsx';
import { UpdatesSection } from './Updates.tsx';
import { UsageSection } from './Usage.tsx';

export function SettingsSectionScreen({ section }: { section: SettingsSection }): ReactNode {
  switch (section) {
    case 'notifications':
      return <NotificationsSection />;
    case 'device':
      return <DeviceSection />;
    case 'usage':
      return <UsageSection />;
    case 'omp':
      return <OmpSettingsSection />;
    case 'updates':
      return <UpdatesSection />;
    case 'backup':
      return <BackupSection />;
    case 'sync':
      return <SyncSection />;
  }
}
