// 两三个选项之间切换的小开关：表单里的「按数量 / 按资金」，划分里的「挂单排布 / 持仓累计」。

import { cn } from '@/lib/utils'

/** 两种设置方式的切换。 */
export function ModeToggle<T extends string>({
  label,
  value,
  options,
  disabled,
  onChange,
}: {
  label: string
  value: T
  options: [T, string][]
  disabled: boolean
  onChange: (value: T) => void
}) {
  return (
    <div role="radiogroup" aria-label={label} className="inline-flex rounded-md bg-muted p-0.5">
      {options.map(([option, text]) => (
        <button
          key={option}
          type="button"
          role="radio"
          aria-checked={value === option}
          disabled={disabled}
          onClick={() => onChange(option)}
          className={cn(
            'rounded px-2 py-0.5 text-[11px] text-muted-foreground transition-colors disabled:opacity-50',
            value === option ? 'bg-card text-foreground shadow-sm ring-1 ring-border' : 'hover:text-foreground',
          )}
        >
          {text}
        </button>
      ))}
    </div>
  )
}
