import type { ReactNode } from 'react';

type StatusBadgeTone = 'neutral' | 'accent' | 'warning' | 'danger' | 'success';

export interface StatusBadgeProps {
  tone?: StatusBadgeTone;
  children: ReactNode;
}

export function StatusBadge({ tone = 'neutral', children }: StatusBadgeProps) {
  return <span className={`status-badge status-badge--${tone}`}>{children}</span>;
}
