import { useCallback, useEffect, useRef, useState } from 'react'
import { LogIn, RefreshCw, RotateCw, UserPlus } from 'lucide-react'
import {
  api,
  type DeviceSignInStartV2,
  type ImportedPeerSummaryV2,
  type PlatformAccountStatusV2,
  type PlatformPeerV2,
  type PlatformTunnelV2,
} from './client-api'
import { truncateName, tunnelPickerLabel } from './peer-label'

/**
 * Signing in, choosing a Tunnel, and adding a Peer — without leaving the app.
 *
 * Importing a `.peer` by hand stays exactly where it was; this is the other
 * door, for an owner who has an account and does not want to visit a website,
 * download a file, and find it again on this machine.
 *
 * It lives in its own file because the Connection tab is already long, and
 * because a phone does not draw it at all: `capabilities.platformAccount`
 * decides, and the caller is the one holding that flag.
 */
interface PlatformOnboardingProps {
  /** Every profile this device already holds, to warn before one is replaced. */
  profiles: ImportedPeerSummaryV2[]
  /** True while a connection exists, when changing profiles is not allowed. */
  locked: boolean
  onImported: (summary: ImportedPeerSummaryV2) => void | Promise<void>
}

type Panel = 'closed' | 'signing-in' | 'creating'

export function PlatformOnboarding({ profiles, locked, onImported }: PlatformOnboardingProps) {
  const [account, setAccount] = useState<PlatformAccountStatusV2 | null>(null)
  const [panel, setPanel] = useState<Panel>('closed')
  const [signIn, setSignIn] = useState<DeviceSignInStartV2 | null>(null)
  const [tunnels, setTunnels] = useState<PlatformTunnelV2[] | null>(null)
  const [selectedTunnel, setSelectedTunnel] = useState('')
  // '' means "mint a new one"; anything else is an already-issued Peer.
  const [selectedPeer, setSelectedPeer] = useState('')
  const [peers, setPeers] = useState<PlatformPeerV2[] | null>(null)
  const [peerName, setPeerName] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  // Bumped by the refresh control to re-run the Peer read.
  const [refreshCounter, setRefreshCounter] = useState(0)

  const refreshAccount = useCallback(async () => {
    try {
      setAccount(await api.platformAccountStatus())
    } catch {
      // A host too old to answer simply has no account panel.
      setAccount(null)
    }
  }, [])

  useEffect(() => {
    void refreshAccount()
  }, [refreshAccount])

  // Polling is owned by this panel, so closing it stops the polling. A timer
  // that outlived the panel would keep asking the Platform about a code the
  // owner can no longer see.
  const pollTimer = useRef<number | null>(null)
  const pollSeconds = useRef(5)
  const stopPolling = useCallback(() => {
    if (pollTimer.current !== null) {
      window.clearTimeout(pollTimer.current)
      pollTimer.current = null
    }
  }, [])
  useEffect(() => stopPolling, [stopPolling])

  const beginSignIn = async () => {
    setBusy(true)
    setError(null)
    setNotice(null)
    try {
      const started = await api.platformStartSignIn()
      setSignIn(started)
      setPanel('signing-in')
      stopPolling()
      pollSeconds.current = Math.max(1, started.interval)

      // A self-rescheduling timeout rather than an interval, because the
      // Platform can ask for a slower cadence mid-flight and RFC 8628 §3.5
      // says to honour it by adding five seconds. An interval cannot change
      // its own period, so a Client on one would answer `slow_down` forever
      // at the speed that caused it.
      const scheduleNextPoll = () => {
        pollTimer.current = window.setTimeout(() => {
          void (async () => {
            try {
              const outcome = await api.platformPollSignIn()
              if (outcome.state === 'slow_down') {
                pollSeconds.current += 5
                scheduleNextPoll()
                return
              }
              if (outcome.state === 'pending' || outcome.state === 'not_started') {
                scheduleNextPoll()
                return
              }
              stopPolling()
              if (outcome.state === 'granted') {
                setSignIn(null)
                setPanel('closed')
                setNotice(outcome.email ? `Signed in as ${outcome.email}.` : 'Signed in.')
                await refreshAccount()
                return
              }
              setSignIn(null)
              setPanel('closed')
              setError(
                outcome.state === 'denied'
                  ? 'That sign-in was denied.'
                  : 'That code expired before it was approved.',
              )
            } catch (pollError) {
              stopPolling()
              setPanel('closed')
              setSignIn(null)
              setError(String(pollError))
            }
          })()
        }, pollSeconds.current * 1000)
      }
      scheduleNextPoll()
    } catch (startError) {
      setError(String(startError))
    } finally {
      setBusy(false)
    }
  }

  const cancelSignIn = () => {
    stopPolling()
    setSignIn(null)
    setPanel('closed')
    void api.platformSignOut().catch(() => undefined)
  }

  const signOut = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.platformSignOut()
      setTunnels(null)
      setSelectedTunnel('')
      setPanel('closed')
      setNotice('Signed out.')
      await refreshAccount()
    } catch (signOutError) {
      setError(String(signOutError))
    } finally {
      setBusy(false)
    }
  }

  /**
   * Re-reads the Tunnel list from the Platform.
   *
   * Always a fresh read, never a cache: a Tunnel created in the browser a
   * moment ago has to be selectable here without restarting the app.
   */
  const loadTunnels = useCallback(async (keepSelection: boolean) => {
    setBusy(true)
    setError(null)
    try {
      const listed = await api.platformListTunnels()
      setTunnels(listed)
      setSelectedTunnel((current) => {
        if (keepSelection && listed.some((tunnel) => tunnel.tunnel_id === current)) return current
        return listed.find(usable)?.tunnel_id ?? listed[0]?.tunnel_id ?? ''
      })
      return true
    } catch (listError) {
      setError(String(listError))
      // An expired session is reported by the host as such; re-reading the
      // account here turns the message into a working Sign in button.
      await refreshAccount()
      return false
    } finally {
      setBusy(false)
    }
  }, [refreshAccount])

  const openCreate = async () => {
    setNotice(null)
    if (await loadTunnels(false)) setPanel('creating')
  }

  // The Peers of whichever Tunnel is selected, refetched when it changes so the
  // list can never belong to a different Tunnel than the one named above it.
  useEffect(() => {
    if (panel !== 'creating' || !selectedTunnel) {
      setPeers(null)
      return
    }
    let cancelled = false
    setPeers(null)
    setSelectedPeer('')
    void (async () => {
      try {
        const listed = await api.platformListPeers(selectedTunnel)
        if (!cancelled) setPeers(listed)
      } catch {
        // A Tunnel whose Peers cannot be listed can still take a new one, so
        // this degrades to the create-only panel rather than failing it.
        if (!cancelled) setPeers([])
      }
    })()
    return () => {
      cancelled = true
    }
  }, [panel, selectedTunnel, refreshCounter])

  const chosen = tunnels?.find((tunnel) => tunnel.tunnel_id === selectedTunnel)
  const replacing = profiles.some((profile) => profile.tunnel_id === selectedTunnel)

  const existing = peers?.find((peer) => peer.peer_id === selectedPeer)
  const creatingNew = selectedPeer === ''
  const canSubmit = Boolean(chosen) && (creatingNew ? peerName.trim().length > 0 : Boolean(existing))

  /**
   * Adopts the chosen Peer, or mints one when "New Peer" is selected.
   *
   * Adopting matters: a machine whose Peer was added from the Console already
   * has an identity, and minting a second one would leave the first orphaned on
   * the Platform, still counted against the Tunnel.
   */
  const submitPeer = async () => {
    if (!chosen || !canSubmit) return
    setBusy(true)
    setError(null)
    try {
      const summary = creatingNew
        ? await api.platformCreatePeer(chosen.tunnel_id, chosen.name ?? null, peerName.trim())
        : await api.platformImportPeer(
            chosen.tunnel_id,
            existing!.peer_id,
            chosen.name ?? null,
            existing!.name ?? null,
          )
      setPanel('closed')
      setPeerName('')
      setNotice(
        creatingNew
          ? `Added ${summary.peer_name ?? 'the Peer'} at ${summary.overlay_ip}.`
          : `Imported ${summary.peer_name ?? 'the Peer'} at ${summary.overlay_ip}.`,
      )
      await onImported(summary)
    } catch (submitError) {
      setError(String(submitError))
    } finally {
      setBusy(false)
    }
  }

  if (!account) return null

  return (
    <div className="space-y-2 border-t border-border pt-3">
      {account.signed_in ? (
        // Stacked rather than side by side: the address is what tells two
        // accounts apart, and sharing one row with two buttons truncated it to
        // `owner…`, which identifies nothing. Sign out is a rare action and
        // reads as one; adding a Peer is the reason this panel exists.
        <div className="space-y-2">
          <div className="flex items-baseline justify-between gap-2">
            <span className="min-w-0 truncate text-sm text-content-muted">
              {account.email ? (
                <>
                  Signed in as{' '}
                  <span className="text-content-secondary">{account.email}</span>
                </>
              ) : (
                'Signed in'
              )}
            </span>
            <button
              type="button"
              onClick={() => void signOut()}
              disabled={busy}
              className="shrink-0 border-none bg-transparent !px-0 !py-0 text-xs text-content-muted underline underline-offset-2 hover:text-content-secondary"
            >
              Sign out
            </button>
          </div>
          <button
            type="button"
            onClick={() => void openCreate()}
            disabled={busy || locked}
            className="w-full border border-accent/30 bg-accent-soft text-accent hover:bg-accent/15"
          >
            <UserPlus className="mr-1 inline h-4 w-4" />
            Add a Peer
          </button>
        </div>
      ) : (
        <div className="space-y-2">
          <p className="text-sm text-content-muted">
            Sign in to add this device to any of your Tunnels — no file to
            download. Every account includes a free Tunnel.
          </p>
          <button
            type="button"
            onClick={() => void beginSignIn()}
            disabled={busy || locked}
            className="w-full border border-accent/30 bg-accent-soft text-accent hover:bg-accent/15"
          >
            {busy ? (
              <RefreshCw className="mx-auto h-4 w-4 animate-spin" />
            ) : (
              <>
                <LogIn className="mr-1 inline h-4 w-4" />
                Sign in
              </>
            )}
          </button>
        </div>
      )}

      {panel === 'signing-in' && signIn && (
        <div className="rounded-lg border border-border bg-surface-subdued p-3 space-y-2">
          <p className="text-sm text-content-secondary">
            {signIn.browser_opened
              ? 'Approve this Client in the browser that just opened.'
              : 'Open this address in any browser and approve this Client:'}
          </p>
          <p className="break-all font-mono text-xs text-content-muted">{signIn.verification_uri}</p>
          <p className="text-xs uppercase tracking-wide text-content-muted">Confirm this code matches</p>
          <p className="font-mono text-2xl tracking-widest">{signIn.user_code}</p>
          <div className="flex items-center gap-2">
            <RefreshCw className="h-4 w-4 animate-spin text-content-muted" />
            <span className="text-sm text-content-muted">Waiting for approval…</span>
          </div>
          <button
            type="button"
            onClick={cancelSignIn}
            className="w-full !py-2 border border-border bg-surface text-content-secondary"
          >
            Cancel
          </button>
        </div>
      )}

      {panel === 'creating' && (
        <div className="rounded-lg border border-border bg-surface-subdued p-3 space-y-3">
          {tunnels && tunnels.length > 0 ? (
            <>
              <label className="block text-sm font-medium">
                <span className="flex items-center justify-between gap-2">
                  Tunnel
                  <button
                    type="button"
                    onClick={() => {
                      setNotice(null)
                      setRefreshCounter((count) => count + 1)
                      void loadTunnels(true)
                    }}
                    disabled={busy}
                    title="Re-read Tunnels and Peers from the Platform"
                    className="shrink-0 border-none bg-transparent !px-0 !py-0 text-xs font-normal text-content-muted underline underline-offset-2 hover:text-content-secondary"
                  >
                    <RotateCw className={`mr-1 inline h-3 w-3 ${busy ? 'animate-spin' : ''}`} />
                    Refresh
                  </button>
                </span>
                <select
                  value={selectedTunnel}
                  onChange={(event) => setSelectedTunnel(event.target.value)}
                  disabled={busy}
                  className="mt-1 w-full"
                >
                  {tunnels.map((tunnel) => (
                    <option
                      key={tunnel.tunnel_id}
                      value={tunnel.tunnel_id}
                      disabled={!usable(tunnel)}
                    >
                      {tunnelPickerLabel(
                        tunnel.name,
                        tunnel.tunnel_id,
                        tunnel.billing?.effective_plan,
                      )}
                      {usable(tunnel) ? '' : ' — inactive'}
                    </option>
                  ))}
                </select>
              </label>

              <label className="block text-sm font-medium">
                Peer
                <select
                  value={selectedPeer}
                  onChange={(event) => setSelectedPeer(event.target.value)}
                  disabled={busy || peers === null}
                  className="mt-1 w-full"
                >
                  <option value="">
                    {peers === null ? 'Loading Peers…' : 'New Peer…'}
                  </option>
                  {(peers ?? []).map((peer) => (
                    <option key={peer.peer_id} value={peer.peer_id}>
                      {peerPickerLabel(peer)}
                    </option>
                  ))}
                </select>
              </label>

              {creatingNew ? (
                <label className="block text-sm font-medium">
                  Name this device
                  <input
                    value={peerName}
                    onChange={(event) => setPeerName(event.target.value)}
                    placeholder="laptop"
                    maxLength={64}
                    disabled={busy}
                    className="mt-1 w-full"
                  />
                </label>
              ) : (
                <p className="text-xs text-content-muted">
                  Brings down the profile this Peer already has. The device using
                  it is not disturbed.
                </p>
              )}

              {replacing && (
                <p className="text-xs text-status-warning">
                  This device already holds a profile for that Tunnel. Adding a Peer replaces it
                  here — the old Peer stays on the Platform until you remove it there.
                </p>
              )}

              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => void submitPeer()}
                  disabled={busy || !canSubmit || !usable(chosen!)}
                  className="flex-1 bg-accent text-content-inverse border-none"
                >
                  {busy ? (
                    <RefreshCw className="mx-auto h-4 w-4 animate-spin" />
                  ) : creatingNew ? (
                    'Add and import'
                  ) : (
                    'Import this Peer'
                  )}
                </button>
                <button
                  type="button"
                  onClick={() => setPanel('closed')}
                  disabled={busy}
                  className="flex-1 border border-border bg-surface text-content-secondary"
                >
                  Cancel
                </button>
              </div>
            </>
          ) : (
            <p className="text-sm text-content-muted">
              This account has no Tunnel yet. Create one on the Platform, then come back.
            </p>
          )}
          <a
            href={`${account.platform_url}/dashboard`}
            target="_blank"
            rel="noreferrer"
            className="block text-xs text-accent underline underline-offset-2"
          >
            Create a new Tunnel on lantunnel.app →
          </a>
        </div>
      )}

      {error && <p className="text-xs text-status-danger">{error}</p>}
      {notice && !error && <p className="text-xs text-content-muted">{notice}</p>}
    </div>
  )
}

/** `laptop - 198.18.0.7`, truncated so a long name cannot overflow the row. */
function peerPickerLabel(peer: PlatformPeerV2): string {
  const name = peer.name?.trim()
    ? truncateName(peer.name, 22)
    : `Peer ${peer.peer_id.slice(0, 8)}`
  return peer.overlay_ip ? `${name} - ${peer.overlay_ip}` : name
}

/** A Tunnel that is disabled or unpaid answers 402 at create time. */
function usable(tunnel: PlatformTunnelV2): boolean {
  const enabled = (tunnel.status ?? 'enabled') === 'enabled'
  const active = (tunnel.billing?.access ?? 'active') === 'active'
  return enabled && active
}
