// 新建与编辑草稿：保存、预览、确认启动（设计第 2.4 节，B42/B43）。
//
// 确认启动之前一张单都不下：下单只发生在带令牌的确认请求里，而令牌只能来自一次预览。

import { useEffect, useState } from 'react'
import { useQuery } from '@tanstack/react-query'

import { HttpError, api } from './api'
import {
  DEFAULT_FORM,
  DraftForm,
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
        message: '行情、仓位或账户在预览之后变了：请重新预览，再确认启动。',
        stalePreview: true,
      }
    case 'instrument_busy':
      return {
        message: '这个标的已经有运行中或已暂停的策略：先停掉那一个，再回来确认启动。',
        stalePreview: false,
      }
    case 'busy':
      return { message: '这个策略正在走一步，稍等一下再点确认启动。', stalePreview: false }
    default:
      return { message: failure.message, stalePreview: false }
  }
}

interface DraftEditorProps {
  /** 已有草稿的标识；新建时为 null。 */
  draftId: string | null
  onStarted: (strategyId: string) => void
  onBack: () => void
}

export function DraftEditor({ draftId, onStarted, onBack }: DraftEditorProps) {
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
      [direction]: { ...current[direction], [name]: value },
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
        setNotice('草稿已保存。')
        return created.draft_id
      }
      const saved = await api.saveDraft(id, config)
      setNotice(`已保存为第 ${saved.version} 版。`)
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
      setNotice(
        result.validation.valid
          ? '预览已按当前行情和账户算好，确认后按它启动。'
          : '参数校验不通过，改完再预览。',
      )
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

  return (
    <section className="editor">
      <h2>{id === null ? '新建策略' : `草稿 ${id}`}</h2>
      <DraftForm
        values={values}
        disabled={busy}
        onInstrument={changeInstrument}
        onChange={changeSide}
        onToggle={toggleSide}
      />
      <div className="actions">
        <button type="button" onClick={onBack} disabled={busy}>
          返回
        </button>
        <button type="button" onClick={save} disabled={busy}>
          保存
        </button>
        <button type="button" onClick={saveAndPreview} disabled={busy} className="primary">
          保存并预览
        </button>
        <button
          type="button"
          onClick={confirmStart}
          disabled={busy || preview?.token == null}
          className="primary"
        >
          确认启动
        </button>
        {id === null ? null : (
          <button type="button" onClick={remove} disabled={busy} className="danger">
            删除草稿
          </button>
        )}
      </div>
      {notice === null ? null : <p className="notice">{notice}</p>}
      {error === null ? null : <p className="error">{error}</p>}
      {preview === null ? null : <PreviewPanel preview={preview} />}
    </section>
  )
}
