// 新建与编辑草稿：保存、预览、确认启动（设计第 2.4 节，B42/B43）。
//
// 确认启动之前一张单都不下：下单只发生在带令牌的确认请求里，而令牌只能来自一次预览。

import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'

import { PageHeader } from '@/components/layout'
import { Button } from '@/components/ui/button'
import { HttpError, api } from './api'
import {
  DEFAULT_FORM,
  DraftForm,
  changeSideField,
  fromConfig,
  toConfig,
  type FormValues,
  type SideFieldName,
} from './DraftForm'
import { PreviewPanel } from './PreviewPanel'
import type { Direction, DraftPreview } from './types'

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

/** 确认启动失败时该怎么跟用户说，以及要不要作废旧预览。 */
interface StartFailure {
  message: string
  stalePreview: boolean
}

/**
 * 确认启动的失败几乎都是 409，但原因不同，下一步也不同。
 *
 * 「预览过期」要重新预览，「标的有别的策略」要去停掉那一个，「正忙」只要再点一次。
 * 服务端在响应里给了类别，网页按类别说，不去猜那句话是什么意思。
 */
function describeStartFailure(failure: unknown): StartFailure {
  if (!(failure instanceof HttpError)) {
    return { message: describe(failure), stalePreview: false }
  }
  switch (failure.kind) {
    case 'preview_stale':
      return {
        message: '预览后行情或仓位有变，重新预览。',
        stalePreview: true,
      }
    case 'instrument_busy':
      return {
        message: '该标的已有活动策略，先停止它。',
        stalePreview: false,
      }
    case 'busy':
      return { message: '正在执行上一步，稍后再试。', stalePreview: false }
    default:
      return { message: failure.message, stalePreview: false }
  }
}

interface DraftEditorProps {
  /** 已有草稿的标识；新建时为 null。 */
  draftId: string | null
  onStarted: (strategyId: string) => void
  onBack: () => void
  /** 新建的草稿第一次保存成功，拿到了标识。 */
  onCreated?: (draftId: string) => void
}

export function DraftEditor({ draftId, onStarted, onBack, onCreated }: DraftEditorProps) {
  const [id, setId] = useState(draftId)
  const [values, setValues] = useState<FormValues>(DEFAULT_FORM)
  const [preview, setPreview] = useState<DraftPreview | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [loaded, setLoaded] = useState<string | null>(null)

  const draft = useQuery({
    queryKey: ['draft', id],
    queryFn: () => api.draft(id as string),
    enabled: id !== null,
  })

  // 打开已有草稿时填一次表单；之后用户改了什么都不再覆盖。
  useEffect(() => {
    const stored = draft.data
    if (stored === undefined || stored.draft_id === loaded) return
    setValues(fromConfig(stored.config))
    setLoaded(stored.draft_id)
  }, [draft.data, loaded])

  /** 参数变了，旧预览就作废：确认启动必须基于改动后的预览（B43）。 */
  function changed() {
    setPreview(null)
    setNotice(null)
    setError(null)
  }

  function changeInstrument(value: string) {
    setValues((current) => ({ ...current, instrument: value }))
    changed()
  }

  function changeSide(direction: Direction, name: SideFieldName, value: string) {
    setValues((current) => ({
      ...current,
      [direction]: changeSideField(current[direction], name, value),
    }))
    changed()
  }

  function toggleSide(direction: Direction, enabled: boolean) {
    setValues((current) => ({
      ...current,
      [direction]: { ...current[direction], enabled },
    }))
    changed()
  }

  /** 保存参数；成功时返回草稿标识。 */
  async function save(): Promise<string | null> {
    const built = toConfig(values)
    if ('problem' in built) {
      setError(built.problem)
      return null
    }
    setBusy(true)
    setError(null)
    try {
      const config = built.config
      if (id === null) {
        const created = await api.createDraft(config)
        setId(created.draft_id)
        setLoaded(created.draft_id)
        onCreated?.(created.draft_id)
        setNotice('草稿已保存。')
        return created.draft_id
      }
      const saved = await api.saveDraft(id, config)
      setNotice(`已保存（v${saved.version}）。`)
      return id
    } catch (failure) {
      setError(describe(failure))
      return null
    } finally {
      setBusy(false)
    }
  }

  async function saveAndPreview() {
    const saved = await save()
    if (saved === null) return
    setBusy(true)
    try {
      const result = await api.previewDraft(saved)
      setPreview(result)
      // 通过时不另说：底栏会写确认后挂几张单。
      setNotice(result.validation.valid ? null : '校验不通过。')
    } catch (failure) {
      setError(describe(failure))
    } finally {
      setBusy(false)
    }
  }

  async function confirmStart() {
    if (id === null || preview?.token == null) return
    setBusy(true)
    setError(null)
    try {
      const started = await api.startDraft(id, preview.token)
      onStarted(started.strategy_id)
    } catch (failure) {
      const { message, stalePreview } = describeStartFailure(failure)
      if (stalePreview) setPreview(null)
      setError(message)
    } finally {
      setBusy(false)
    }
  }

  async function remove() {
    if (id === null) return
    setBusy(true)
    try {
      await api.deleteDraft(id)
      onBack()
    } catch (failure) {
      setError(describe(failure))
    } finally {
      setBusy(false)
    }
  }

  const built = toConfig(values)
  const previewConfig = 'config' in built ? built.config : null
  const ready = preview?.token != null

  return (
    <div className="flex flex-col gap-6 pb-20">
      <div className="flex flex-col gap-2">
        <Button variant="ghost" size="sm" className="-ml-2 w-fit text-muted-foreground" onClick={onBack} disabled={busy}>
          ← 返回
        </Button>
        <PageHeader
          title={id === null ? '新建策略' : '编辑草稿'}
          subtitle={id === null ? undefined : <span className="num">{id}</span>}
          actions={
            id === null ? null : (
              <Button variant="destructive" size="lg" onClick={remove} disabled={busy}>
                删除草稿
              </Button>
            )
          }
        />
      </div>

      <DraftForm
        values={values}
        disabled={busy}
        onInstrument={changeInstrument}
        onChange={changeSide}
        onToggle={toggleSide}
      />

      {preview === null ? null : <PreviewPanel preview={preview} config={previewConfig} />}

      {/* 操作栏固定在底部：滚到哪里都看得到下一步，以及点下去会发生什么。 */}
      <div className="fixed inset-x-0 bottom-0 z-30 border-t border-border bg-background/90 backdrop-blur">
        <div className="mx-auto flex max-w-[1240px] flex-wrap items-center gap-3 px-5 py-3">
          <div className="min-w-0 flex-1 text-xs">
            {error !== null ? (
              <span role="alert" className="text-destructive">{error}</span>
            ) : notice !== null ? (
              <span className="text-muted-foreground">{notice}</span>
            ) : ready ? (
              <span className="text-muted-foreground">
                确认后挂 <span className="num text-foreground">{preview.planned_orders.length}</span> 张
                {preview.immediate_fills.length > 0 ? (
                  <>
                    ，<span className="num text-warn">{preview.immediate_fills.length}</span> 张立即成交
                  </>
                ) : null}
              </span>
            ) : (
              <span className="text-muted-foreground">预览后可确认启动</span>
            )}
          </div>
          <Button variant="ghost" size="lg" onClick={save} disabled={busy}>
            保存
          </Button>
          <Button variant="outline" size="lg" onClick={saveAndPreview} disabled={busy}>
            保存并预览
          </Button>
          <Button size="lg" onClick={confirmStart} disabled={busy || !ready}>
            确认启动
          </Button>
        </div>
      </div>
    </div>
  )
}
