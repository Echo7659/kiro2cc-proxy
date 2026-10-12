import { useId, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { useQueryClient } from '@tanstack/react-query'
import { AlertCircle, CheckCircle2, Loader2 } from 'lucide-react'
import { importProxies } from '@/api/credentials'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { ImportProxiesResponse } from '@/types/api'

interface ProxyImportDialogProps {
  onClose: () => void
}

/** Mounted only while open: credentials stay in this local form, never in query/storage caches. */
export function ProxyImportDialog({ onClose }: ProxyImportDialogProps) {
  const { t } = useTranslation()
  const inputId = useId()
  const client = useQueryClient()
  const [draft, setDraft] = useState('')
  const [pending, setPending] = useState(false)
  const [failed, setFailed] = useState(false)
  const [result, setResult] = useState<ImportProxiesResponse | null>(null)

  function close() {
    setDraft('')
    onClose()
  }

  async function submit() {
    setPending(true)
    setFailed(false)
    setResult(null)
    try {
      const response = await importProxies(draft)
      // A confirmed response ends this import, including partial rejections.
      setDraft('')
      setResult(response)
      client.invalidateQueries({ queryKey: ['proxies'] })
      client.invalidateQueries({ queryKey: ['credentials'] })
    } catch {
      // Preserve the draft for retry; never display Axios/server text containing input credentials.
      setFailed(true)
    } finally {
      setPending(false)
    }
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) close()
      }}
    >
      <DialogContent className="max-h-[90vh] max-w-2xl overflow-y-auto">
        <DialogHeader>
          <DialogTitle>{t('proxies.importTitle')}</DialogTitle>
          <DialogDescription>
            {t('proxies.importDescription')}
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-2">
          <label
            htmlFor={inputId}
            className="text-[12px] font-medium text-ink-2"
          >
            {t('proxies.importInput')}
          </label>
          <textarea
            id={inputId}
            value={draft}
            onChange={(event) => {
              setDraft(event.target.value)
              setFailed(false)
            }}
            placeholder="gateway.example:10001:demo-user:demo~password"
            autoComplete="off"
            autoCorrect="off"
            spellCheck={false}
            disabled={pending}
            className="h-52 w-full resize-y rounded-[8px] border border-hairline-2 bg-surface-2 p-3 font-mono text-[12px] leading-relaxed text-ink placeholder:text-ink-3 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-brand disabled:opacity-60"
          />
          <p className="text-[11.5px] leading-relaxed text-ink-3">
            {t('proxies.importPrivacy')}
          </p>
        </div>
        {failed && (
          <div
            role="alert"
            className="flex items-start gap-2 rounded-[8px] border border-danger-line bg-danger-soft p-3 text-[12px] text-danger"
          >
            <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
            {t('proxies.importFailed')}
          </div>
        )}
        {result && (
          <div
            role="status"
            className="space-y-3 rounded-[8px] border border-hairline bg-surface-2 p-3 text-[12px]"
          >
            <div className="flex items-start gap-2 text-ink-2">
              <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-brand" />
              <span>
                {t('proxies.importSummary', {
                  imported: result.imported,
                  duplicates: result.duplicates,
                  rejected: result.rejected,
                })}
              </span>
            </div>
            {result.imported > 0 && (
              <p className="text-ink-3">{t('proxies.importChecking')}</p>
            )}
            {result.errors.length > 0 && (
              <ul className="max-h-40 space-y-1 overflow-y-auto text-danger">
                {result.errors.map((error, index) => (
                  <li key={`${error.line}-${index}`}>
                    {t('proxies.lineRejected', { line: error.line })}
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={close}>
            {t('common.close')}
          </Button>
          <Button onClick={submit} disabled={pending || !draft.trim()}>
            {pending && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
            {pending ? t('proxies.importing') : t('proxies.importButton')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
