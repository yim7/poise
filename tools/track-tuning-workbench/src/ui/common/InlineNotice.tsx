import type { ReactNode } from 'react';

type InlineNoticeTone = 'info' | 'warning' | 'danger';

export interface InlineNoticeProps {
  tone?: InlineNoticeTone;
  title?: string;
  children: ReactNode;
}

export function InlineNotice({
  tone = 'info',
  title,
  children,
}: InlineNoticeProps) {
  return (
    <div
      className={`inline-notice inline-notice--${tone}`}
      role={tone === 'danger' ? 'alert' : 'status'}
    >
      {title ? <p className="inline-notice__title">{title}</p> : null}
      <div className="inline-notice__body">{children}</div>
    </div>
  );
}
