import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'

import { RangeBar } from '@/components/range-bar'
import { config, longSide } from '@/test/fixtures'
import type { CellSummary, StrategyConfig } from '@/types'

function cell(index: number, entry: string, held: string, quantity = '0.01'): CellSummary {
  return {
    direction: 'long',
    index,
    batch: null,
    entry_price: entry,
    exit_price: String(Number(entry) + 1000),
    quantity,
    held,
    to_buy_back: '0',
    working_orders: 0,
    status: 'holding',
    profit_maker: '0',
    profit_taker: '0',
  }
}

/** 多头侧 79000–81500、间距 500：五格，最下面两格空，中间一格持有 40%，上面两格满。 */
const fiveCells: StrategyConfig = {
  ...config,
  long: {
    ...longSide,
    price_low: '79000',
    price_high: '81500',
    grid_spacing: '500',
    invalidation: { price: '76000', remaining: '0', max_slippage: '0.01' },
  },
}
const cells = [
  cell(0, '81500', '0.01'),
  cell(1, '81000', '0.01'),
  cell(2, '80500', '0.004'),
  cell(3, '80000', '0'),
  cell(4, '79500', '0'),
]

function slots(container: HTMLElement) {
  return [...container.querySelectorAll<HTMLElement>('[data-held]')]
}

describe('RangeBar', () => {
  it('每一格按持有比例填色，部分持有只填左边那一截', () => {
    const { container } = render(<RangeBar config={fiveCells} cells={cells} mark="80200" />)

    const drawn = slots(container)
    expect(drawn.map((slot) => slot.dataset.held)).toEqual(['1', '1', '0.4', '0', '0'])
    expect((drawn[2].firstElementChild as HTMLElement).style.width).toBe('40%')
    expect(drawn[3].firstElementChild).toBeNull()
    // 格子之间留空隙：每格左右各让 1px。
    expect(drawn[0].style.left).toContain('+ 1px')
  })

  it('详情页标出失效价、均价、保本价和标记价', () => {
    render(
      <RangeBar
        config={fiveCells}
        cells={cells}
        mark="80200"
        average="80980"
        breakeven="80720"
        variant="full"
      />,
    )

    expect(screen.getByText('失效 76000')).toBeInTheDocument()
    expect(screen.getByText('保本 80720')).toBeInTheDocument()
    expect(screen.getByText('均价 80980')).toBeInTheDocument()
    expect(screen.getByText('80200')).toBeInTheDocument()
    expect(screen.getByTitle('持仓均价 80980')).toBeInTheDocument()
    expect(screen.getByTitle('保本价 80720')).toBeInTheDocument()
  })

  it('标记价贴着失效价时只写标记价，失效价留红线和悬停提示', () => {
    render(<RangeBar config={fiveCells} cells={cells} mark="76050" variant="full" />)

    expect(screen.getByText('76050')).toBeInTheDocument()
    expect(screen.queryByText('失效 76000')).not.toBeInTheDocument()
    expect(screen.getByTitle('观点失效价 76000')).toBeInTheDocument()
  })

  it('紧凑版只标范围两端和标记价', () => {
    render(<RangeBar config={fiveCells} cells={cells} mark="80200" average="80980" breakeven="80720" />)

    expect(screen.getByText('80200')).toBeInTheDocument()
    expect(screen.getByText('79000')).toBeInTheDocument()
    expect(screen.queryByText('失效 76000')).not.toBeInTheDocument()
    expect(screen.queryByText(/均价/)).not.toBeInTheDocument()
  })

  it('一侧格子太密时不留空隙', () => {
    const dense = Array.from({ length: 80 }, (_, index) => cell(index, String(95000 - index * 250), '0'))
    const { container } = render(
      <RangeBar config={{ ...config, long: { ...longSide, grid_spacing: '250' } }} cells={dense} />,
    )

    const drawn = slots(container)
    expect(drawn).toHaveLength(80)
    expect(drawn[0].style.left).toContain('+ 0px')
  })

  it('没有格子表时每一侧画成一整格空的', () => {
    const { container } = render(<RangeBar config={config} mark="85000" />)

    expect(slots(container).map((slot) => slot.dataset.held)).toEqual(['0'])
  })
})
