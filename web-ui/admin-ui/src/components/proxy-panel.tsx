import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import {
  Activity,
  AlertCircle,
  CheckCircle2,
  Globe2,
  Loader2,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
} from 'lucide-react'
import { toast } from 'sonner'
import { PageHead } from '@/components/page-head'
import { ProxyImportDialog } from '@/components/proxy-import-dialog'
import { SearchBox, Segmented, Toolbar, UpdatedAgo } from '@/components/toolbar'
import {
  CELL,
  ICON_BTN,
  PANEL,
  PANEL_FOOT,
  Pager,
  TH_BASE,
} from '@/components/table-kit'
import {
  ACTION_BTN,
  ACTION_BTN_PRIMARY,
} from '@/components/dashboard/panel-constants'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Switch } from '@/components/ui/switch'
import { useProxies, useProxyActions } from '@/hooks/use-proxies'
import { localeTag } from '@/lib/locale'
import type { ProxySummary } from '@/types/api'

type Filter = 'all' | 'healthy' | 'unhealthy' | 'unknown' | 'disabled'
const PAGE_SIZE = 25

function hasCooldown(proxy: ProxySummary): boolean {
  return !!proxy.cooldownUntil && Date.parse(proxy.cooldownUntil) > Date.now()
}

function isAvailable(proxy: ProxySummary): boolean {
  return proxy.enabled && proxy.status === 'healthy' && !hasCooldown(proxy)
}

function matchesFilter(proxy: ProxySummary, filter: Filter): boolean {
  if (filter === 'all') return true
  if (filter === 'disabled') return !proxy.enabled
  if (!proxy.enabled) return false
  if (filter === 'healthy') return isAvailable(proxy)
  if (filter === 'unhealthy')
    return proxy.status === 'unhealthy' || hasCooldown(proxy)
  return proxy.status === 'unknown'
}

function formatDate(value: string | null): string {
  if (!value) return '—'
  const date = new Date(value)
  return Number.isNaN(date.getTime()) ? '—' : date.toLocaleString(localeTag())
}

export function ProxyPanel() {
  const { t } = useTranslation()
  const { data, isLoading, isError, isFetching, dataUpdatedAt, refetch } =
    useProxies()
  const actions = useProxyActions()
  const [importOpen, setImportOpen] = useState(false)
  const [deleting, setDeleting] = useState<ProxySummary | null>(null)
  const [search, setSearch] = useState('')
  const [filter, setFilter] = useState<Filter>('all')
  const [requestedPage, setPage] = useState(1)
  const proxies = data?.proxies ?? []
  const enabled = proxies.filter((proxy) => proxy.enabled).length
  const healthy = proxies.filter(isAvailable).length
  const shared = proxies.filter((proxy) => proxy.assignedAccounts > 1).length
  const needle = search.trim().toLowerCase()
  const filtered = proxies.filter((proxy) => {
    return (
      matchesFilter(proxy, filter) &&
      `${proxy.id} ${proxy.url} ${proxy.exitIp ?? ''}`
        .toLowerCase()
        .includes(needle)
    )
  })
  const totalPages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE))
  const page = Math.min(requestedPage, totalPages)
  const rows = filtered.slice((page - 1) * PAGE_SIZE, page * PAGE_SIZE)
  const busyIds = new Set([
    actions.check.isPending ? actions.check.variables : undefined,
    actions.toggle.isPending ? actions.toggle.variables?.id : undefined,
    actions.remove.isPending ? actions.remove.variables : undefined,
  ])

  async function checkAll() {
    try {
      await actions.checkAll.mutateAsync()
      toast.success(t('proxies.checkScheduled'))
    } catch {
      toast.error(t('proxies.actionFailed'))
    }
  }

  async function check(id: number) {
    try {
      await actions.check.mutateAsync(id)
      toast.success(t('proxies.checkScheduled'))
    } catch {
      toast.error(t('proxies.actionFailed'))
    }
  }

  async function toggle(proxy: ProxySummary, nextEnabled: boolean) {
    try {
      await actions.toggle.mutateAsync({ id: proxy.id, enabled: nextEnabled })
    } catch {
      toast.error(t('proxies.actionFailed'))
    }
  }

  async function remove() {
    if (!deleting) return
    try {
      await actions.remove.mutateAsync(deleting.id)
      setDeleting(null)
      toast.success(t('proxies.deleted'))
    } catch {
      toast.error(t('proxies.actionFailed'))
    }
  }

  function status(proxy: ProxySummary) {
    if (!proxy.enabled)
      return { label: t('proxies.disabled'), tone: 'bg-surface-3 text-ink-3' }
    if (proxy.checking)
      return { label: t('proxies.checking'), tone: 'bg-brand-soft text-brand' }
    if (hasCooldown(proxy))
      return {
        label: t('proxies.cooldown'),
        tone: 'bg-danger-soft text-danger',
      }
    if (proxy.status === 'healthy')
      return { label: t('proxies.healthy'), tone: 'bg-ok-soft text-ok' }
    if (proxy.status === 'unhealthy')
      return {
        label: t('proxies.unhealthy'),
        tone: 'bg-danger-soft text-danger',
      }
    return { label: t('proxies.unknown'), tone: 'bg-surface-3 text-ink-3' }
  }

  const summaries = [
    { label: t('proxies.total'), value: proxies.length, icon: Globe2 },
    { label: t('proxies.enabledCount'), value: enabled, icon: ShieldCheck },
    { label: t('proxies.healthyCount'), value: healthy, icon: CheckCircle2 },
    { label: t('proxies.sharedCount'), value: shared, icon: Activity },
  ]

  return (
    <div className="min-w-0">
      <PageHead
        crumb={[t('dashboard.navSystem'), t('proxies.title')]}
        title={t('proxies.title')}
        note={t('proxies.headNote')}
        actions={
          <>
            <button
              type="button"
              className={ACTION_BTN}
              onClick={() => refetch()}
              disabled={isFetching}
              aria-label={t('common.refresh')}
            >
              <RefreshCw className={isFetching ? 'animate-spin' : ''} />
              {t('common.refresh')}
            </button>
            <button
              type="button"
              className={ACTION_BTN}
              onClick={checkAll}
              disabled={!enabled || actions.checkAll.isPending}
            >
              {actions.checkAll.isPending ? (
                <Loader2 className="animate-spin" />
              ) : (
                <Activity />
              )}
              {t('proxies.checkAll')}
            </button>
            <button
              type="button"
              className={ACTION_BTN_PRIMARY}
              onClick={() => setImportOpen(true)}
            >
              <Plus />
              {t('proxies.importButton')}
            </button>
          </>
        }
      />
      <div className="mb-4 grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {summaries.map(({ label, value, icon: Icon }) => (
          <div
            key={label}
            className="rounded-[11px] border border-hairline bg-surface px-4 py-3 shadow-hair"
          >
            <div className="mb-2 flex items-center gap-1.5 text-[12px] text-ink-3">
              <Icon className="h-3.5 w-3.5" />
              {label}
            </div>
            <div className="text-[26px] font-bold tabular-nums tracking-tight text-ink">
              {data ? value.toLocaleString(localeTag()) : '—'}
            </div>
          </div>
        ))}
      </div>
      <div className="space-y-1 rounded-[9px] border border-brand-line bg-brand-soft px-3 py-2.5 text-[12px] leading-relaxed text-ink-2">
        <p>{t('proxies.assignmentNote')}</p>
        <p className="text-ink-3">{t('proxies.egressNote')}</p>
      </div>
      {data?.configured && healthy === 0 && (
        <div
          role="status"
          className="mt-3 flex items-start gap-2 rounded-[9px] border border-danger-line bg-danger-soft px-3 py-2.5 text-[12px] leading-relaxed text-danger"
        >
          <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
          <p>{t('proxies.noHealthy')}</p>
        </div>
      )}
      {isError && (
        <div
          role="alert"
          className="mt-3 flex flex-wrap items-center gap-2 text-[12px] text-danger"
        >
          <AlertCircle className="h-4 w-4" />
          {t('proxies.loadFailed')}
          <Button variant="outline" size="sm" onClick={() => refetch()}>
            {t('common.retry')}
          </Button>
        </div>
      )}
      <Toolbar>
        <SearchBox
          value={search}
          onChange={(value) => {
            setSearch(value)
            setPage(1)
          }}
          placeholder={t('proxies.searchPlaceholder')}
          clearLabel={t('proxies.clearSearch')}
          widthClass="w-full sm:w-[260px]"
        />
        <div className="max-w-full overflow-x-auto">
          <Segmented<Filter>
            value={filter}
            onChange={(value) => {
              setFilter(value)
              setPage(1)
            }}
            groupLabel={t('proxies.filterLabel')}
            options={[
              { key: 'all', label: t('proxies.all') },
              { key: 'healthy', label: t('proxies.healthy') },
              { key: 'unhealthy', label: t('proxies.unhealthy') },
              { key: 'unknown', label: t('proxies.unknown') },
              { key: 'disabled', label: t('proxies.disabled') },
            ]}
          />
        </div>
        <div className="ml-auto text-[11.5px] text-ink-3">
          <UpdatedAgo dataUpdatedAt={dataUpdatedAt} />
        </div>
      </Toolbar>
      <section className={PANEL} aria-label={t('proxies.title')}>
        <div className="max-h-[65vh] overflow-auto">
          <table className="w-full min-w-[950px] text-[12px]">
            <thead>
              <tr>
                {[
                  'colStatus',
                  'colAddress',
                  'colAuth',
                  'colExitIp',
                  'colLatency',
                  'colAccounts',
                  'colChecked',
                  'colActions',
                ].map((key) => (
                  <th key={key} scope="col" className={TH_BASE}>
                    {t(`proxies.${key}`)}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((proxy) => {
                const currentStatus = status(proxy)
                const busy = busyIds.has(proxy.id)
                return (
                  <tr key={proxy.id} className="hover:bg-surface-2">
                    <td className={CELL}>
                      <span
                        className={`inline-flex items-center gap-1 rounded-md px-2 py-1 text-[10.5px] font-medium ${currentStatus.tone}`}
                      >
                        {proxy.checking && proxy.enabled && (
                          <Loader2 className="h-3 w-3 animate-spin" />
                        )}
                        {currentStatus.label}
                      </span>
                    </td>
                    <td className={CELL}>
                      <div className="font-mono text-[11.5px] text-ink">
                        {proxy.url}
                      </div>
                      <div className="mt-1 text-[10px] text-ink-3">
                        #{proxy.id}
                      </div>
                    </td>
                    <td className={CELL}>
                      <div>
                        {proxy.hasAuth
                          ? t('proxies.authConfigured')
                          : t('proxies.noAuth')}
                      </div>
                      {proxy.hasAuth && proxy.usernameMasked && (
                        <div className="mt-1 font-mono text-[10.5px] text-ink-3">
                          {proxy.usernameMasked}
                        </div>
                      )}
                    </td>
                    <td className={`${CELL} font-mono text-[11.5px]`}>
                      {proxy.exitIp || '—'}
                    </td>
                    <td className={`${CELL} tabular-nums`}>
                      {proxy.latencyMs === null ? '—' : `${proxy.latencyMs} ms`}
                    </td>
                    <td className={CELL}>
                      <span
                        className="tabular-nums"
                        title={(proxy.assignedAccountIds ?? [])
                          .map((id) => `#${id}`)
                          .join(', ')}
                      >
                        {t('proxies.accountsCount', {
                          count: proxy.assignedAccounts,
                        })}
                      </span>
                      {proxy.assignedAccounts > 1 && (
                        <div className="mt-1 text-[10px] text-ink-3">
                          {t('proxies.shared')}
                        </div>
                      )}
                    </td>
                    <td
                      className={`${CELL} whitespace-nowrap text-[11px] text-ink-3`}
                    >
                      <div>{formatDate(proxy.lastCheckedAt)}</div>
                      {hasCooldown(proxy) && (
                        <div className="mt-1 text-danger">
                          {t('proxies.retryAt', {
                            time: formatDate(proxy.cooldownUntil),
                          })}
                        </div>
                      )}
                    </td>
                    <td className={CELL}>
                      <div className="flex items-center gap-2">
                        <Switch
                          checked={proxy.enabled}
                          onCheckedChange={(checked) => toggle(proxy, checked)}
                          disabled={busy}
                          aria-label={t('proxies.toggleEnabled', {
                            id: proxy.id,
                          })}
                          className="data-[state=checked]:bg-brand data-[state=unchecked]:bg-hairline-2"
                        />
                        <button
                          type="button"
                          className={`${ICON_BTN} disabled:opacity-40`}
                          onClick={() => check(proxy.id)}
                          disabled={busy || proxy.checking || !proxy.enabled}
                          title={t('proxies.checkOne', { id: proxy.id })}
                          aria-label={t('proxies.checkOne', { id: proxy.id })}
                        >
                          {proxy.checking ||
                          (actions.check.isPending &&
                            actions.check.variables === proxy.id) ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" />
                          ) : (
                            <Activity className="h-3.5 w-3.5" />
                          )}
                        </button>
                        <button
                          type="button"
                          className={`${ICON_BTN} hover:bg-danger-soft hover:text-danger disabled:opacity-40`}
                          onClick={() => setDeleting(proxy)}
                          disabled={busy}
                          title={t('proxies.deleteOne', { id: proxy.id })}
                          aria-label={t('proxies.deleteOne', { id: proxy.id })}
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      </div>
                    </td>
                  </tr>
                )
              })}
              {!rows.length && (
                <tr>
                  <td
                    colSpan={8}
                    className="p-10 text-center text-[12px] text-ink-3"
                  >
                    {isLoading ? (
                      <span className="inline-flex items-center gap-2">
                        <Loader2 className="h-4 w-4 animate-spin" />
                        {t('common.loading')}
                      </span>
                    ) : proxies.length ? (
                      t('proxies.noMatches')
                    ) : (
                      t('proxies.empty')
                    )}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        <div className={PANEL_FOOT}>
          <span>{t('proxies.listCount', { count: filtered.length })}</span>
          <div className="ml-auto flex items-center gap-2">
            <Pager page={page} totalPages={totalPages} onPage={setPage} />
            <span>{t('credentials.footPerPage', { size: PAGE_SIZE })}</span>
          </div>
        </div>
      </section>
      {importOpen && <ProxyImportDialog onClose={() => setImportOpen(false)} />}
      <Dialog
        open={deleting !== null}
        onOpenChange={(open) => {
          if (!open) setDeleting(null)
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{t('proxies.deleteTitle')}</DialogTitle>
            <DialogDescription>
              {t('proxies.deleteDescription', {
                id: deleting?.id,
                count: deleting?.assignedAccounts ?? 0,
              })}
            </DialogDescription>
          </DialogHeader>
          <p className="rounded-[8px] border border-danger-line bg-danger-soft p-3 text-[12px] leading-relaxed text-danger">
            {t('proxies.deleteLastWarning')}
          </p>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleting(null)}>
              {t('common.cancel')}
            </Button>
            <Button
              variant="destructive"
              onClick={remove}
              disabled={actions.remove.isPending}
            >
              {actions.remove.isPending && (
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              )}
              {t('common.delete')}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}
