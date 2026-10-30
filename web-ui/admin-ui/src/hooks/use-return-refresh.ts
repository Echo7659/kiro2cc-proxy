// Copyright (c) 2026 Harllan He. Licensed under MIT.
import { useEffect, useRef } from 'react'

/** 同一次"切回"可能同时触发 visibilitychange 与 focus，该窗口内的重复触发合并为一次 */
const DEDUP_WINDOW_MS = 1000

/**
 * 用户从别处切回当前页面时触发 onReturn。覆盖两种场景：
 * - 浏览器切换 tab / 最小化后恢复：visibilitychange（hidden → visible）
 * - 浏览器窗口仍可见但失去过焦点（如 Cmd+Tab 切到其他应用 / 其他窗口再切回）：window focus
 *
 * enabled 为 false 时不挂监听（用于只在特定页面生效）；onReturn 经 ref 持有最新闭包，调用方无需 memo。
 */
export function useReturnRefresh(enabled: boolean, onReturn: () => void) {
  const onReturnRef = useRef(onReturn)
  useEffect(() => {
    onReturnRef.current = onReturn
  })

  useEffect(() => {
    if (!enabled) return
    let last = 0
    const fire = () => {
      const now = Date.now()
      if (now - last < DEDUP_WINDOW_MS) return
      last = now
      onReturnRef.current()
    }
    const onVisibility = () => {
      if (document.visibilityState === 'visible') fire()
    }
    document.addEventListener('visibilitychange', onVisibility)
    window.addEventListener('focus', fire)
    return () => {
      document.removeEventListener('visibilitychange', onVisibility)
      window.removeEventListener('focus', fire)
    }
  }, [enabled])
}
