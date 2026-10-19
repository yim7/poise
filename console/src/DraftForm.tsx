// 新建/编辑草稿的参数表单：多头侧与空头侧各一块，两侧都可选。
//
// 表单里全是字符串，原样交给服务端校验：前端不判断参数是否合理，也不算数量。
// 只有两件事在表单里定：哪几侧要配置（至少一侧，服务端也会拒绝两侧都空的配置），
// 以及两侧都配置时主仓比例按 0 提交——中性网格的主仓比例必须是 0，让用户填一个
// 必定被拒的值没有意义。

import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { cn } from '@/lib/utils'
import { DIRECTION_LABELS } from './labels'
import { invalidationInput, parseInvalidation, parseThrottle, throttleMinutes } from './format'
import type { Direction, Invalidation, SideConfig, StrategyConfig } from './types'

/** 一侧的表单值。 */
export interface SideFormValues {
  enabled: boolean
  priceLow: string
  priceHigh: string
  maxPosition: string
  coreRatio: string
  batchCount: string
  gridSpacing: string
  gridSpread: string
  throttleMinutes: string
  /** 失效价；留空表示不启用观点失效。 */
  invalidationPrice: string
  /** 失效后剩余数量；留空按 0。 */
  invalidationRemaining: string
  /** 失效平仓滑点上限（%）。 */
  invalidationSlippage: string
  /**
   * 网格价差相对间距的比例 g / d，默认 1。改价差时记下，改间距时价差按它跟着变。单独存着，
   * 不从输入框现算：把间距删空重新输入的那一刻，输入框里算不出比例。
   */
  spreadRatio: number
}

/** 一侧表单里能编辑的字段：`enabled` 是单独的开关，`spreadRatio` 由价差推出来，都不走输入框。 */
export type SideFieldName = Exclude<keyof SideFormValues, 'enabled' | 'spreadRatio'>

function positive(value: string): number | null {
  const parsed = Number(value.trim())
  return value.trim() !== '' && Number.isFinite(parsed) && parsed > 0 ? parsed : null
}

/**
 * 改一侧的一个字段。网格价差按相对间距的比例跟随：默认 g = d；改过价差就记下 g / d，
 * 之后改间距时价差按这个比例重新算。间距还不是正数（删空了、正在输入）时价差不动。
 */
export function changeSideField(
  values: SideFormValues,
  name: SideFieldName,
  value: string,
): SideFormValues {
  const next = { ...values, [name]: value }
  if (name === 'gridSpread') {
    const spacing = positive(values.gridSpacing)
    const spread = positive(value)
    if (spacing !== null && spread !== null) next.spreadRatio = spread / spacing
  }
  if (name === 'gridSpacing') {
    const spacing = positive(value)
    if (spacing !== null) next.gridSpread = String(Number((spacing * values.spreadRatio).toPrecision(12)))
  }
  return next
}

/** 价差下面的一行说明：现在按什么比例跟随间距。写成公式就够：改 d 时价差跟着变，一看便知。 */
export function spreadHint(ratio: number): string {
  if (ratio === 1) return '= d'
  return `= ${Number(ratio.toPrecision(4))} × d`
}

export interface FormValues {
  instrument: string
  long: SideFormValues
  short: SideFormValues
}

/** 设计第 3.2 节的例子：填好就能预览。空头侧默认不启用，价格范围在多头上方。 */
export const DEFAULT_FORM: FormValues = {
  instrument: 'BTC',
  long: {
    enabled: true,
    priceLow: '75000',
    priceHigh: '95000',
    maxPosition: '1',
    coreRatio: '0',
    batchCount: '',
    gridSpacing: '1000',
    gridSpread: '1000',
    throttleMinutes: '',
    invalidationPrice: '',
    invalidationRemaining: '',
    invalidationSlippage: '1',
    spreadRatio: 1,
  },
  short: {
    enabled: false,
    priceLow: '96000',
    priceHigh: '99000',
    maxPosition: '1',
    coreRatio: '0',
    batchCount: '',
    gridSpacing: '1000',
    gridSpread: '1000',
    throttleMinutes: '',
    invalidationPrice: '',
    invalidationRemaining: '',
    invalidationSlippage: '1',
    spreadRatio: 1,
  },
}

/** 表单变成配置的结果：要么是一份配置，要么是一句说给用户听的话。 */
export type ConfigInput = { config: StrategyConfig } | { problem: string }

/** 一侧的转换结果：要么是配置（没启用这一侧时是 null），要么是一句提示。 */
type SideInput = { config: SideConfig | null } | { problem: string }

/**
 * 表单变成配置。
 *
 * 参数合不合理由服务端判断，这里只管把输入表达出来；只有限速那一项表达不出来
 * （填了不是正数的东西），才在这里拦下——它没法既保留用户填的内容又提交出去。
 */
export function toConfig(values: FormValues): ConfigInput {
  const neutral = values.long.enabled && values.short.enabled
  const long = sideConfig('long', values.long, neutral)
  if ('problem' in long) return long
  const short = sideConfig('short', values.short, neutral)
  if ('problem' in short) return short
  return {
    config: {
      instrument: values.instrument.trim(),
      long: long.config,
      short: short.config,
    },
  }
}

function sideConfig(
  direction: Direction,
  values: SideFormValues,
  neutral: boolean,
): SideInput {
  if (!values.enabled) return { config: null }
  const throttle = parseThrottle(values.throttleMinutes)
  if ('problem' in throttle) {
    return { problem: `${DIRECTION_LABELS[direction]}侧：${throttle.problem}` }
  }
  const batchCount = values.batchCount.trim()
  const invalidation = invalidationConfig(direction, values, neutral)
  if ('problem' in invalidation) return invalidation
  return {
    config: {
      price_low: values.priceLow.trim(),
      price_high: values.priceHigh.trim(),
      max_position: values.maxPosition.trim(),
      // 中性网格两侧都不设主仓（第 5.2 节）。
      core_ratio: neutral ? '0' : values.coreRatio.trim(),
      batch_count: neutral || batchCount === '' ? null : Number(batchCount),
      grid_spacing: values.gridSpacing.trim(),
      grid_spread: values.gridSpread.trim(),
      throttle_interval: throttle.interval,
      invalidation: invalidation.invalidation,
    },
  }
}

/** 失效设置：解析规则见 `parseInvalidation`。 */
function invalidationConfig(
  direction: Direction,
  values: SideFormValues,
  neutral: boolean,
): { invalidation: Invalidation | null } | { problem: string } {
  const parsed = parseInvalidation(
    {
      price: values.invalidationPrice,
      remaining: values.invalidationRemaining,
      slippage: values.invalidationSlippage,
    },
    neutral,
  )
  return 'problem' in parsed ? { problem: `${DIRECTION_LABELS[direction]}侧：${parsed.problem}` } : parsed
}

export function fromConfig(config: StrategyConfig): FormValues {
  return {
    instrument: config.instrument,
    long: sideValues(config.long, DEFAULT_FORM.long),
    short: sideValues(config.short, DEFAULT_FORM.short),
  }
}

/** 失效设置换回表单里的三个输入框。 */
function invalidationFields(
  invalidation: Invalidation | null,
): Pick<SideFormValues, 'invalidationPrice' | 'invalidationRemaining' | 'invalidationSlippage'> {
  const input = invalidationInput(invalidation)
  return {
    invalidationPrice: input.price,
    invalidationRemaining: input.remaining,
    invalidationSlippage: input.slippage,
  }
}

/** 存下来的草稿里价差和间距的比例；算不出来按 1。 */
function savedRatio(spacing: string, spread: string): number {
  const d = positive(spacing)
  const g = positive(spread)
  return d === null || g === null ? 1 : g / d
}

/** 没配置的这一侧回到默认值：下次勾上它时不是一片空白。 */
function sideValues(side: SideConfig | null, fallback: SideFormValues): SideFormValues {
  if (side === null) return { ...fallback, enabled: false }
  return {
    enabled: true,
    priceLow: side.price_low,
    priceHigh: side.price_high,
    maxPosition: side.max_position,
    coreRatio: side.core_ratio,
    batchCount: side.batch_count === null ? '' : String(side.batch_count),
    gridSpacing: side.grid_spacing,
    gridSpread: side.grid_spread,
    throttleMinutes: throttleMinutes(side.throttle_interval),
    ...invalidationFields(side.invalidation),
    spreadRatio: savedRatio(side.grid_spacing, side.grid_spread),
  }
}

interface FieldProps {
  direction: Direction
  name: SideFieldName
  label: string
  /** 输入框下面的一行小字。 */
  hint?: string
  /** 输入框里的占位提示。 */
  placeholder?: string
  disabled: boolean
  values: SideFormValues
  onChange: (direction: Direction, name: SideFieldName, value: string) => void
}

function Field({ direction, name, label, hint, placeholder, disabled, values, onChange }: FieldProps) {
  return (
    <label className="flex min-w-0 flex-col gap-1">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <Input
        name={`${direction}_${name}`}
        aria-label={label}
        className="num h-8"
        value={values[name]}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(event) => onChange(direction, name, event.target.value)}
      />
      {hint === undefined ? null : <span className="text-[10px] leading-snug text-muted-foreground/80">{hint}</span>}
    </label>
  )
}

/** 一组相关的参数：小标题加一行输入框。 */
function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-2">
      <div className="eyebrow">{title}</div>
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">{children}</div>
    </div>
  )
}

interface SideFieldsProps {
  direction: Direction
  values: SideFormValues
  /** 两侧都配置：这一份是中性网格，主仓比例必须是 0。 */
  neutral: boolean
  disabled: boolean
  onChange: (direction: Direction, name: SideFieldName, value: string) => void
  onToggle: (direction: Direction, enabled: boolean) => void
}

function SideFields({
  direction,
  values,
  neutral,
  disabled,
  onChange,
  onToggle,
}: SideFieldsProps) {
  const label = DIRECTION_LABELS[direction]
  const coreDisabled = disabled || neutral
  // 中性网格固定按「无主仓、全部平仓」提交：输入框显示要提交的值，不显示改成中性之前
  // 填的那个——否则看到的和存下的不是一份参数。原来的值留着，取消中性时还在。
  const shown = neutral
    ? { ...values, coreRatio: '0', batchCount: '', invalidationRemaining: '0' }
    : values
  const field = (name: SideFieldName, fieldLabel: string, extra: Partial<FieldProps> = {}) => (
    <Field
      direction={direction}
      name={name}
      label={fieldLabel}
      disabled={disabled}
      values={shown}
      onChange={onChange}
      {...extra}
    />
  )
  return (
    <fieldset
      className={cn(
        'min-w-0 rounded-lg bg-card ring-1 ring-border',
        values.enabled && (direction === 'long' ? 'border-t-2 border-t-long' : 'border-t-2 border-t-short'),
      )}
    >
      <legend className="sr-only">{label}侧</legend>
      <div className="flex items-center justify-between gap-3 px-4 py-3">
        <span
          className={cn(
            'font-heading text-sm font-semibold',
            values.enabled ? (direction === 'long' ? 'text-long' : 'text-short') : 'text-muted-foreground',
          )}
        >
          {label}侧
        </span>
        <label className="flex items-center">
          <Switch
            name={`${direction}_enabled`}
            aria-label={`启用${label}侧`}
            checked={values.enabled}
            disabled={disabled}
            onCheckedChange={(checked) => onToggle(direction, checked)}
          />
        </label>
      </div>
      {/* 没启用的这一侧收起来：值留着（打开还在），少看一半字段。 */}
      {values.enabled ? (
        <div className="flex flex-col gap-4 border-t border-border px-4 py-4">
          <Group title="价格与仓位">
            {field('priceLow', '价格下限 L')}
            {field('priceHigh', '价格上限 H')}
            {field('maxPosition', '最大仓位 M')}
          </Group>
          <Group title="主仓">
            {field('coreRatio', '主仓比例 r', {
              disabled: coreDisabled,
              hint: neutral ? '中性固定 0' : undefined,
            })}
            {field('batchCount', '主仓批数 n', { disabled: coreDisabled, hint: 'r = 0 时留空' })}
          </Group>
          <Group title="网格">
            {field('gridSpacing', '网格间距 d')}
            {field('gridSpread', '网格价差 g', { hint: spreadHint(values.spreadRatio) })}
          </Group>
          <Group title="限速与止损">
            {field('throttleMinutes', '限速间隔 τ（分钟）', { placeholder: '不限速' })}
            {field('invalidationPrice', '观点失效价', {
              placeholder: '不止损',
              hint: direction === 'long' ? '须低于 L' : '须高于 H',
            })}
            {field('invalidationRemaining', '失效后剩余数量 R', {
              disabled: disabled || neutral,
              hint: neutral ? '中性固定 0' : undefined,
            })}
            {field('invalidationSlippage', '失效平仓滑点上限（%）')}
          </Group>
        </div>
      ) : null}
    </fieldset>
  )
}

interface DraftFormProps {
  values: FormValues
  disabled: boolean
  onInstrument: (value: string) => void
  onChange: (direction: Direction, name: SideFieldName, value: string) => void
  onToggle: (direction: Direction, enabled: boolean) => void
}

export function DraftForm({
  values,
  disabled,
  onInstrument,
  onChange,
  onToggle,
}: DraftFormProps) {
  const neutral = values.long.enabled && values.short.enabled
  return (
    <div className="flex flex-col gap-4">
      <label className="flex max-w-sm flex-col gap-1">
        <span className="text-[11px] text-muted-foreground">标的</span>
        <Input
          name="instrument"
          aria-label="标的"
          className="num h-9 text-sm font-semibold"
          value={values.instrument}
          disabled={disabled}
          onChange={(event) => onInstrument(event.target.value)}
        />
        <span className="text-[10px] text-muted-foreground/80">HIP-3 市场写 xyz:GOLD</span>
      </label>
      <div className="grid grid-cols-1 items-start gap-3 lg:grid-cols-2">
        <SideFields
          direction="long"
          values={values.long}
          neutral={neutral}
          disabled={disabled}
          onChange={onChange}
          onToggle={onToggle}
        />
        <SideFields
          direction="short"
          values={values.short}
          neutral={neutral}
          disabled={disabled}
          onChange={onChange}
          onToggle={onToggle}
        />
      </div>
      {neutral ? (
        <p className="text-xs text-info">中性网格不设主仓</p>
      ) : null}
    </div>
  )
}
