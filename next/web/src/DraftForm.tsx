// 新建/编辑草稿的参数表单。
//
// 表单里全是字符串，原样交给服务端校验：前端不判断参数是否合理，也不算数量。

import { parseThrottle, throttleMinutes } from './format'
import type { StrategyConfig } from './types'

export interface FormValues {
  instrument: string
  priceLow: string
  priceHigh: string
  maxPosition: string
  coreRatio: string
  batchCount: string
  gridSpacing: string
  gridSpread: string
  throttleMinutes: string
}

/** 设计第 3.2 节的例子：填好就能预览。 */
export const DEFAULT_FORM: FormValues = {
  instrument: 'BTC',
  priceLow: '75000',
  priceHigh: '95000',
  maxPosition: '1',
  coreRatio: '0',
  batchCount: '',
  gridSpacing: '1000',
  gridSpread: '1000',
  throttleMinutes: '',
}

/** 表单变成配置的结果：要么是一份配置，要么是一句说给用户听的话。 */
export type ConfigInput = { config: StrategyConfig } | { problem: string }

/**
 * 表单变成配置。
 *
 * 参数合不合理由服务端判断，这里只管把输入表达出来；只有限速那一项表达不出来
 * （填了不是正数的东西），才在这里拦下——它没法既保留用户填的内容又提交出去。
 */
export function toConfig(values: FormValues): ConfigInput {
  const throttle = parseThrottle(values.throttleMinutes)
  if ('problem' in throttle) return throttle
  const batchCount = values.batchCount.trim()
  return {
    config: {
      instrument: values.instrument.trim(),
      long: {
        price_low: values.priceLow.trim(),
        price_high: values.priceHigh.trim(),
        max_position: values.maxPosition.trim(),
        core_ratio: values.coreRatio.trim(),
        batch_count: batchCount === '' ? null : Number(batchCount),
        grid_spacing: values.gridSpacing.trim(),
        grid_spread: values.gridSpread.trim(),
        throttle_interval: throttle.interval,
      },
    },
  }
}

export function fromConfig(config: StrategyConfig): FormValues {
  return {
    instrument: config.instrument,
    priceLow: config.long.price_low,
    priceHigh: config.long.price_high,
    maxPosition: config.long.max_position,
    coreRatio: config.long.core_ratio,
    batchCount: config.long.batch_count === null ? '' : String(config.long.batch_count),
    gridSpacing: config.long.grid_spacing,
    gridSpread: config.long.grid_spread,
    throttleMinutes: throttleMinutes(config.long.throttle_interval),
  }
}

interface FieldProps {
  name: keyof FormValues
  label: string
  hint?: string
  disabled: boolean
  values: FormValues
  onChange: (name: keyof FormValues, value: string) => void
}

function Field({ name, label, hint, disabled, values, onChange }: FieldProps) {
  return (
    <label className="field">
      <span>{label}</span>
      <input
        name={name}
        aria-label={label}
        value={values[name]}
        disabled={disabled}
        onChange={(event) => onChange(name, event.target.value)}
      />
      {hint === undefined ? null : <small>{hint}</small>}
    </label>
  )
}

interface DraftFormProps {
  values: FormValues
  disabled: boolean
  onChange: (name: keyof FormValues, value: string) => void
}

export function DraftForm({ values, disabled, onChange }: DraftFormProps) {
  return (
    <div className="form">
      <Field
        name="instrument"
        label="标的"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <label className="field">
        <span>方向</span>
        <select value="long" disabled onChange={() => undefined}>
          <option value="long">做多</option>
        </select>
        <small>做空与中性由任务 07 加入</small>
      </label>
      <Field
        name="priceLow"
        label="价格下限 L"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="priceHigh"
        label="价格上限 H"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="maxPosition"
        label="最大仓位 M"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="coreRatio"
        label="主仓比例 r"
        hint="0 表示没有主仓，1 表示只积累主仓"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="batchCount"
        label="主仓批数 n"
        hint="r = 0 时留空"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="gridSpacing"
        label="网格间距 d"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="gridSpread"
        label="网格价差 g"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
      <Field
        name="throttleMinutes"
        label="限速间隔 τ（分钟）"
        hint="留空表示不限速"
        disabled={disabled}
        values={values}
        onChange={onChange}
      />
    </div>
  )
}
