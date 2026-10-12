import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useEffect } from 'react'
import {
  checkProxies,
  checkProxy,
  deleteProxy,
  getProxies,
  setProxyEnabled,
} from '@/api/credentials'

export function useProxies(active = true) {
  const client = useQueryClient()
  const query = useQuery({
    queryKey: ['proxies'],
    queryFn: ({ signal }) => getProxies(signal),
    enabled: active,
    retry: false,
    staleTime: 10_000,
    refetchIntervalInBackground: false,
    refetchInterval: (query) => {
      if (!active || query.state.error) return false
      const pending = query.state.data?.proxies.some(
        (proxy) =>
          proxy.checking ||
          (proxy.enabled && proxy.status === 'unknown') ||
          (proxy.enabled &&
            proxy.cooldownUntil &&
            Date.parse(proxy.cooldownUntil) > Date.now()),
      )
      // Checks refresh quickly; a visible configured pool also refreshes runtime failover/bindings.
      if (pending) return 3000
      return query.state.data?.configured ? 10_000 : false
    },
  })
  const configured = query.data?.configured
  const routingState = query.data?.proxies
    .map((proxy) =>
      [
        proxy.id,
        proxy.enabled,
        proxy.status,
        proxy.checking,
        proxy.assignedAccounts,
        ...(proxy.assignedAccountIds ?? []),
      ].join(':'),
    )
    .join('|')
  useEffect(() => {
    if (active && configured)
      client.invalidateQueries({ queryKey: ['credentials'] })
  }, [active, configured, routingState, client])
  return query
}

export function useProxyActions() {
  const client = useQueryClient()
  const invalidate = () => {
    client.invalidateQueries({ queryKey: ['proxies'] })
    client.invalidateQueries({ queryKey: ['credentials'] })
  }
  const checkAll = useMutation({
    mutationFn: checkProxies,
    onSuccess: invalidate,
  })
  const check = useMutation({ mutationFn: checkProxy, onSuccess: invalidate })
  const toggle = useMutation({
    mutationFn: ({ id, enabled }: { id: number; enabled: boolean }) =>
      setProxyEnabled(id, enabled),
    onSuccess: invalidate,
  })
  const remove = useMutation({ mutationFn: deleteProxy, onSuccess: invalidate })
  return { checkAll, check, toggle, remove }
}
