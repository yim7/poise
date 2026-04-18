import '@testing-library/jest-dom/vitest';

import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { AppShell } from '@/app/AppShell';

describe('AppShell', () => {
  it('renders the five main workspace regions', () => {
    render(<AppShell />);

    expect(screen.getByRole('region', { name: '文件操作区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: 'Track 列表区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: '关键指标区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: '主图区' })).toBeInTheDocument();
    expect(screen.getByRole('region', { name: '参数编辑区' })).toBeInTheDocument();
  });
});
