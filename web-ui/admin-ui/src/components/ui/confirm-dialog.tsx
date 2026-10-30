import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'

/**
 * window.confirm 的带动画替代。浏览器原生 confirm 无法做进出场动画，
 * 这里用同一套 Dialog 实现，调用方式保持 `await confirmDialog(msg)`。
 * 需要在应用根部挂载一次 <ConfirmDialogHost />。
 */
type Listener = (message: string) => void

let listener: Listener | null = null
let pending: ((ok: boolean) => void) | null = null

function settle(ok: boolean) {
  const resolve = pending
  pending = null
  resolve?.(ok)
}

export function confirmDialog(message: string): Promise<boolean> {
  // 未挂载 Host 时退回原生 confirm，保证确认逻辑不会被静默跳过
  if (!listener) return Promise.resolve(window.confirm(message))
  settle(false) // 上一个未决的确认视为取消
  return new Promise<boolean>((resolve) => {
    pending = resolve
    listener?.(message)
  })
}

export function ConfirmDialogHost() {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  // 关闭动画期间需要保留文案，所以与 open 分开存
  const [message, setMessage] = useState('')

  useEffect(() => {
    listener = (msg) => {
      setMessage(msg)
      setOpen(true)
    }
    return () => {
      listener = null
      settle(false)
    }
  }, [])

  const close = (ok: boolean) => {
    setOpen(false)
    settle(ok)
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !next && close(false)}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{t('common.confirm')}</DialogTitle>
          <DialogDescription>{message}</DialogDescription>
        </DialogHeader>
        <DialogFooter className="gap-2 sm:gap-0">
          <Button variant="outline" onClick={() => close(false)}>
            {t('common.cancel')}
          </Button>
          <Button onClick={() => close(true)}>{t('common.confirm')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
