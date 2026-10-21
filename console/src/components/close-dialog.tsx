// 市价平仓的确认（第 7.7 节）：停止之后的第二次确认，以及已归档、仍有仓位时的平仓。
//
// 显示要平的净仓位、标记价和最差成交价；默认按钮是「保留仓位」，关掉也是保留。

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Private } from '@/components/privacy'
import { decimal } from '@/format'
import { cn } from '@/lib/utils'

/** 市价平仓相对标记价最多让出 5%（第 7.7 节），和服务端一致。只用来显示最差成交价。 */
const MARKET_CLOSE_SLIPPAGE = 0.05

/**
 * 第二次确认：市价平仓（第 7.7 节）。显示要平的净仓位、标记价和最差成交价；默认按钮是
 * 「保留仓位」，关掉也是保留。
 */
/**
 * 按交易所的价格规则显示：最多 5 位有效数字，整数价格总是允许（附录 A）。实际下单的限价
 * 也按这个规整，这里只是估算，所以写「约」。
 */
function significant(price: number): string {
  const decimals = Math.max(0, 4 - Math.floor(Math.log10(Math.abs(price))))
  return String(Number(price.toFixed(decimals)))
}

export function CloseDialog({
  open,
  onOpenChange,
  position,
  mark,
  onConfirm,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  position: string
  mark: string
  onConfirm: () => void
}) {
  const long = Number(position) > 0
  const worst = Number(mark) * (long ? 1 - MARKET_CLOSE_SLIPPAGE : 1 + MARKET_CLOSE_SLIPPAGE)

  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>市价平仓？</AlertDialogTitle>
          <AlertDialogDescription>
            平掉全部净仓位。只减仓 IOC，限价让出标记价 5%，未成交部分继续发。
          </AlertDialogDescription>
        </AlertDialogHeader>
        <dl className="grid grid-cols-3 gap-3 rounded-md bg-muted/40 p-3 text-xs">
          <div className="flex flex-col gap-1">
            <dt className="eyebrow">净仓位</dt>
            <dd className={cn('num text-base font-medium', long ? 'text-long' : 'text-short')}>
              <Private>{decimal(position)}</Private>
            </dd>
          </div>
          <div className="flex flex-col gap-1">
            <dt className="eyebrow">标记价</dt>
            <dd className="num text-base">{decimal(mark)}</dd>
          </div>
          <div className="flex flex-col gap-1">
            <dt className="eyebrow">最差成交价</dt>
            <dd className="num text-base text-warn">约 {significant(worst)}</dd>
          </div>
        </dl>
        <AlertDialogFooter>
          <AlertDialogCancel>保留仓位</AlertDialogCancel>
          <AlertDialogAction variant="destructive" onClick={onConfirm}>
            市价平仓
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
