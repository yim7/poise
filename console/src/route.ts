// 页面地址：用 hash 记下当前在哪一屏，刷新、前进后退都回到原处。
//
// 屏只有五种，不引入路由库：解析和生成各一个函数就够了。

import { useCallback, useEffect, useState } from 'react'

export type Route =
  | { name: 'list' }
  | { name: 'new' }
  | { name: 'draft'; id: string }
  | { name: 'strategy'; id: string }
  | { name: 'settings' }

export function parseRoute(hash: string): Route {
  const [screen, id] = hash.replace(/^#\/?/, '').split('/')
  if (screen === 'new') return { name: 'new' }
  if (screen === 'settings') return { name: 'settings' }
  if (screen === 'draft' && id) return { name: 'draft', id: decodeURIComponent(id) }
  if (screen === 'strategy' && id) return { name: 'strategy', id: decodeURIComponent(id) }
  return { name: 'list' }
}

export function routeHash(route: Route): string {
  switch (route.name) {
    case 'list':
      return '#/'
    case 'new':
      return '#/new'
    case 'settings':
      return '#/settings'
    case 'draft':
      return `#/draft/${encodeURIComponent(route.id)}`
    case 'strategy':
      return `#/strategy/${encodeURIComponent(route.id)}`
  }
}

/** 当前这一屏，以及换到另一屏的函数。 */
export function useRoute(): [Route, (route: Route) => void] {
  const [route, setRoute] = useState(() => parseRoute(window.location.hash))
  useEffect(() => {
    const onChange = () => setRoute(parseRoute(window.location.hash))
    window.addEventListener('hashchange', onChange)
    return () => window.removeEventListener('hashchange', onChange)
  }, [])
  const navigate = useCallback((next: Route) => {
    window.location.hash = routeHash(next)
    window.scrollTo({ top: 0 })
  }, [])
  return [route, navigate]
}

/**
 * 只换地址栏，不换屏：新建的草稿保存后地址换成这份草稿，页面上的表单和预览原样留着，
 * 刷新时按新地址打开同一份草稿。replaceState 不触发 hashchange，后退也不会回到空表单。
 */
export function replaceHash(route: Route) {
  window.history.replaceState(null, '', routeHash(route))
}
