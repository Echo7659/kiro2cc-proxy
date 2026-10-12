import { useTranslation } from 'react-i18next'
import type { CredentialStatusItem } from '@/types/api'

interface CredentialProxyInfoProps {
  credential: CredentialStatusItem
  compact?: boolean
}

export function CredentialProxyInfo({
  credential,
  compact = false,
}: CredentialProxyInfoProps) {
  const { t } = useTranslation()
  const pooled = credential.proxyMode === 'pool'
  const routeStatus = credential.proxyRouteStatus ?? 'pending'
  const address = credential.effectiveProxyUrl || credential.proxyUrl
  const binding = credential.proxyPoolId
    ? `#${credential.proxyPoolId}`
    : t('proxies.awaitingAssignment')
  const status = t(`proxies.route_${routeStatus}`)

  if (compact) {
    if (!pooled && !(credential.hasProxy && credential.proxyUrl)) return null
    return (
      <span
        title={
          pooled
            ? t('proxies.accountBindingTitle', {
                binding,
                status,
                address: address || '—',
              })
            : t('credentials.proxyLabel', { url: address })
        }
        className={`flex-none rounded-[4px] border px-[3px] text-[9px] font-semibold ${pooled && routeStatus === 'unavailable' ? 'border-danger-line bg-danger-soft text-danger' : 'border-brand-line bg-brand-soft text-brand'}`}
      >
        {pooled ? `${t('proxies.automatic')} ${binding}` : 'PROXY'}
      </span>
    )
  }

  if (!credential.proxyMode && !credential.hasProxy) return null
  return (
    <section className="mb-4 space-y-1 rounded-[9px] border border-hairline bg-surface px-3 py-2.5 text-[12px] leading-relaxed">
      <div className="flex flex-wrap items-center gap-2 text-ink-2">
        <span className="font-medium">{t('proxies.accountRouteLabel')}</span>
        <span>{t(`proxies.mode_${credential.proxyMode || 'manual'}`)}</span>
        {pooled && (
          <span
            className={
              routeStatus === 'unavailable' ? 'text-danger' : 'text-ink-3'
            }
          >
            {binding} · {status}
          </span>
        )}
      </div>
      {address && (
        <p className="break-all font-mono text-[11px] text-ink-3">{address}</p>
      )}
      {pooled && (
        <p
          className={
            routeStatus === 'unavailable' ? 'text-danger' : 'text-ink-3'
          }
        >
          {t(
            routeStatus === 'unavailable'
              ? 'proxies.accountUnavailable'
              : 'proxies.assignmentNote',
          )}
        </p>
      )}
    </section>
  )
}
