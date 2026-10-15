// 新建/编辑草稿的参数表单：多头侧与空头侧各一块，两侧都可选。
//
// 表单里全是字符串，原样交给服务端校验：前端不判断参数是否合理，也不算数量。
// 只有两件事在表单里定：哪几侧要配置（至少一侧，服务端也会拒绝两侧都空的配置），
// 以及两侧都配置时主仓比例按 0 提交——中性网格的主仓比例必须是 0，让用户填一个
// 必定被拒的值没有意义。

import { DIRECTION_LABELS } from './labels'
import { parseThrottle, throttleMinutes } from './format'
import type { Direction, SideConfig, StrategyConfig } from './types'

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
}

/** 一侧表单里能编辑的字段：`enabled` 是单独的开关，不走这些输入框。 */
export type SideFieldName = Exclude<keyof SideFormValues, 'enabled'>

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
    },
  }
}

export function fromConfig(config: StrategyConfig): FormValues {
  return {
    instrument: config.instrument,
    long: sideValues(config.long, DEFAULT_FORM.long),
    short: sideValues(config.short, DEFAULT_FORM.short),
  }
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
  }
}

interface FieldProps {
  direction: Direction
  name: SideFieldName
  label: string
  hint?: string
  disabled: boolean
  values: SideFormValues
  onChange: (direction: Direction, name: SideFieldName, value: string) => void
}

function Field({ direction, name, label, hint, disabled, values, onChange }: FieldProps) {
  return (
    <label className="field">
      <span>{label}</span>
      <input
        name={`${direction}_${name}`}
        aria-label={label}
        value={values[name]}
        disabled={disabled}
        onChange={(event) => onChange(direction, name, event.target.value)}
      />
      {hint === undefined ? null : <small>{hint}</small>}
    </label>
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
  // 没启用的这一侧：值留着（勾回来还在），但改不了。
  const off = disabled || !values.enabled
  const coreDisabled = off || neutral
  return (
    <fieldset className="side">
      <legend>{label}侧</legend>
      <label className="field">
        <input
          type="checkbox"
          name={`${direction}_enabled`}
          checked={values.enabled}
          disabled={disabled}
          onChange={(event) => onToggle(direction, event.target.checked)}
        />
        <span>启用{label}侧</span>
      </label>
      <Field
        direction={direction}
        name="priceLow"
        label="价格下限 L"
        disabled={off}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="priceHigh"
        label="价格上限 H"
        disabled={off}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="maxPosition"
        label="最大仓位 M"
        disabled={off}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="coreRatio"
        label="主仓比例 r"
        hint={
          neutral
            ? '两侧都配置时是中性网格，主仓比例必须是 0'
            : '0 表示没有主仓，1 表示只积累主仓'
        }
        disabled={coreDisabled}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="batchCount"
        label="主仓批数 n"
        hint="r = 0 时留空"
        disabled={coreDisabled}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="gridSpacing"
        label="网格间距 d"
        disabled={off}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="gridSpread"
        label="网格价差 g"
        disabled={off}
        values={values}
        onChange={onChange}
      />
      <Field
        direction={direction}
        name="throttleMinutes"
        label="限速间隔 τ（分钟）"
        hint="留空表示不限速"
        disabled={off}
        values={values}
        onChange={onChange}
      />
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
    <div className="form">
      <label className="field">
        <span>标的</span>
        <input
          name="instrument"
          aria-label="标的"
          value={values.instrument}
          disabled={disabled}
          onChange={(event) => onInstrument(event.target.value)}
        />
      </label>
      <div className="sides">
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
      {neutral ? <p className="hint">两侧都配置：这是中性网格，两侧都不设主仓。</p> : null}
    </div>
  )
}
