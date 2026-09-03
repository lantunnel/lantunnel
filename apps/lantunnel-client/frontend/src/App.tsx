import { useEffect, useId, useRef, useState } from 'react'
import { Wifi, WifiOff, RefreshCw, FileText, Trash2, Check, Copy, Search, ChevronDown, QrCode } from 'lucide-react'
import { api, type ConnectionStatus, type AppSettings, type ProxyStatus, type ProductInfo, type RemotePeerRowV2, type ImportedPeerSummaryV2, type ClientAccessRuleV2 } from './client-api'
import { fallbackCapabilities, loadCapabilities, type Capabilities } from './capabilities'

const defaultSettings: AppSettings = {
  auto_start: false,
  auto_connect: false,
  local_socks5_listen: '127.0.0.1:1080',
  local_proxy_enabled: true,
  p2p_allow_lan_candidates: false,
  local_service_exports: [],
  log_level: 'info',
  client_access: {
    allow: [],
    deny: [],
  },
  exported_lans: [],
  auto_export_current_lan: true,
  tunnel_first: false,
  exported_lan_statuses: [],
}

const emptyStatus: ConnectionStatus = {
  connected: false,
  connecting: false,
  message: 'Disconnected',
  platform_heartbeat: { active: false },
  transport_heartbeat: { active: false },
  uptime_secs: 0,
  path_mode: 'disconnected',
  traffic: {
    relay_tx_bytes: 0,
    relay_rx_bytes: 0,
    p2p_tx_bytes: 0,
    p2p_rx_bytes: 0,
  },
}

const emptyProxyStatus: ProxyStatus = {
  running: false,
  listen_addr: defaultSettings.local_socks5_listen,
  tun_running: false,
  tun_routes: [],
}

const defaultProductInfo: ProductInfo = {
  binary_name: 'lantunnel-client',
  display_name: 'Lantunnel',
  role: 'peer',
  version: 'dev',
}



type AppTab = 'connection' | 'peers' | 'settings' | 'logs'

export default function App() {
  const [status, setStatus] = useState<ConnectionStatus>(emptyStatus)
  const [proxyStatus, setProxyStatus] = useState<ProxyStatus>(emptyProxyStatus)
  const [loading, setLoading] = useState(false)
  const [activeTab, setActiveTab] = useState<AppTab>('connection')
  const [settings, setSettings] = useState<AppSettings>(defaultSettings)
  const [initializing, setInitializing] = useState(true)
  const [logs, setLogs] = useState<string[]>([])
  const [localSocksDraft, setLocalSocksDraft] = useState(defaultSettings.local_socks5_listen)
  const [productInfo, setProductInfo] = useState<ProductInfo>(defaultProductInfo)
  const [clashCopied, setClashCopied] = useState(false)
  const [peerSearch, setPeerSearch] = useState('')
  const [showAllPeers, setShowAllPeers] = useState(false)
  const [peerProfiles, setPeerProfiles] = useState<ImportedPeerSummaryV2[]>([])
  const [selectedTunnelId, setSelectedTunnelId] = useState('')
  const [allowRules, setAllowRules] = useState<ClientAccessRuleV2[]>([])
  const [denyRules, setDenyRules] = useState<ClientAccessRuleV2[]>([])
  const [exportedLansDraft, setExportedLansDraft] = useState('')
  const [caps, setCaps] = useState<Capabilities>(fallbackCapabilities)
  const logContainerRef = useRef<HTMLDivElement>(null)
  const clashCopyTimerRef = useRef<number>()

  useEffect(() => {
    ;(async () => {
      try {
        const product = await api.getProductInfo()
        setProductInfo(product)
        setCaps(await loadCapabilities())
        const [s, profiles] = await Promise.all([
          api.getSettings(),
          api.listPeerProfiles(),
        ])
        const mergedSettings = { ...defaultSettings, ...s }
        const initialStatus = await api.getStatus()
        applySettings(mergedSettings)
        setExportedLansDraft(mergedSettings.exported_lans.join('\n'))
        setPeerProfiles(profiles)
        setSelectedTunnelId(profiles[0]?.tunnel_id || '')
        setLocalSocksDraft(s.local_socks5_listen || defaultSettings.local_socks5_listen)
        setStatus(initialStatus)
        setProxyStatus(await api.getProxyStatus())
      } catch (e) {
        console.error(e)
      } finally {
        setInitializing(false)
      }
    })()
  }, [])

  useEffect(() => {
    let un: (() => void) | undefined
    api.onStatus(() => {
      api.getStatus().then((next) => {
        setStatus(next)
      }).catch(() => {})
      api.getProxyStatus().then(setProxyStatus).catch(() => {})
      api.getSettings()
        .then((next) => applySettings({ ...defaultSettings, ...next }))
        .catch(() => {})
    }).then((fn) => (un = fn))
    return () => un?.()
  }, [])

  useEffect(() => {
    let un: (() => void) | undefined
    api.onLog((line) => setLogs((prev) => [...prev.slice(-999), line])).then((fn) => (un = fn))
    return () => un?.()
  }, [])

  useEffect(() => {
    if (activeTab !== 'logs') return
    let stopped = false
    const refresh = () => {
      api
        .getLogs(500)
        .then((next) => {
          if (!stopped) setLogs(next)
        })
        .catch(() => {})
    }
    refresh()
    const timer = window.setInterval(refresh, 1000)
    return () => {
      stopped = true
      window.clearInterval(timer)
    }
  }, [activeTab])

  useEffect(() => {
    if (logContainerRef.current) {
      logContainerRef.current.scrollTop = logContainerRef.current.scrollHeight
    }
  }, [logs, activeTab])

  useEffect(() => {
    return () => {
      if (clashCopyTimerRef.current) window.clearTimeout(clashCopyTimerRef.current)
    }
  }, [])

  /**
   * The rule list is a view of the saved policy, not a second copy of it.
   *
   * Seeding it once at startup let the two drift: the list and its summary
   * could describe rules the runtime was not enforcing, with nothing on screen
   * saying which was true.
   */
  const applySettings = (next: AppSettings) => {
    setSettings(next)
    setAllowRules(next.client_access.allow)
    setDenyRules(next.client_access.deny)
  }

  const refreshSettingsFromBackend = async (opts: { syncLocalSocks?: boolean; syncLanRoutes?: boolean } = {}) => {
    const latest = { ...defaultSettings, ...(await api.getSettings()) }
    applySettings(latest)
    if (opts.syncLocalSocks) {
      setLocalSocksDraft(latest.local_socks5_listen || defaultSettings.local_socks5_listen)
    }
    return latest
  }

  const handleConnect = async () => {
    if (!selectedTunnelId) {
      alert('Import and select a Peer profile first')
      return
    }
    setLoading(true)
    try {
      await api.connectPeerProfile(selectedTunnelId)
      setProxyStatus(await api.getProxyStatus())
    } catch (e) {
      alert(formatActionError('Connect', e))
    } finally {
      setLoading(false)
    }
  }

  const adoptImportedProfile = async (
    label: string,
    run: () => Promise<ImportedPeerSummaryV2 | null>,
  ) => {
    setLoading(true)
    try {
      const imported = await run()
      // A cancelled picker is not a failure, and must not clear the selection.
      if (!imported) return
      const profiles = await api.listPeerProfiles()
      setPeerProfiles(profiles)
      setSelectedTunnelId(imported.tunnel_id)
    } catch (e) {
      alert(formatActionError(label, e))
    } finally {
      setLoading(false)
    }
  }

  const importPeerProfile = () =>
    adoptImportedProfile('Import Peer profile', () => api.pickPeerProfile())

  const scanPeerProfile = () =>
    adoptImportedProfile('Scan Peer profile', () => api.scanPeerProfile())

  const handleDisconnect = async () => {
    const previousStatus = status
    setLoading(true)
    setStatus({
      ...emptyStatus,
      message: status.connecting ? 'Cancelling...' : 'Disconnecting...',
    })
    try {
      await api.disconnect()
      const [nextStatus, nextProxyStatus] = await Promise.all([
        api.getStatus(),
        api.getProxyStatus(),
      ])
      setStatus(nextStatus)
      setProxyStatus(nextProxyStatus)
    } catch (e) {
      setStatus(previousStatus)
      alert(`Disconnect failed: ${e}`)
      setProxyStatus(await api.getProxyStatus().catch(() => proxyStatus))
    } finally {
      setLoading(false)
    }
  }

  const saveSettingsPatch = async (
    patch: Partial<AppSettings>,
    opts: { syncLocalSocks?: boolean; syncLanRoutes?: boolean } = {},
  ) => {
    const next = { ...settings, ...patch }
    setSettings(next)
    try {
      await api.saveSettings(next)
      await refreshSettingsFromBackend(opts)
      setProxyStatus(await api.getProxyStatus())
      return true
    } catch (e) {
      alert(formatActionError('Save settings', e))
      setSettings(settings)
      return false
    }
  }

  /**
   * Removes an imported profile.
   *
   * A Tunnel could be joined but never left — nothing here could remove one,
   * and importing the same Tunnel again was refused, so a reinstall or a
   * re-issued profile had no way in.
   */
  const forgetProfile = async (tunnelId: string) => {
    if (!window.confirm('Remove this Peer profile from this device? The Tunnel is unaffected; import the file again to rejoin.')) return
    setLoading(true)
    try {
      const remaining = await api.forgetPeerProfile(tunnelId)
      setPeerProfiles(remaining)
      setSelectedTunnelId(remaining[0]?.tunnel_id || '')
    } catch (e) {
      alert(formatActionError('Remove Peer profile', e))
    } finally {
      setLoading(false)
    }
  }

  const saveClientAccess = async (allow: ClientAccessRuleV2[], deny: ClientAccessRuleV2[]) => {
    const saved = await saveSettingsPatch({ client_access: { allow, deny } })
    if (saved) {
      // saveSettingsPatch already applied what the backend returned; writing the
      // request back over it is how the list starts disagreeing with the runtime.
    }
    return saved
  }

  const saveExportedLans = async () => {
    const exportedLans = exportedLansDraft
      .split(/\r?\n/)
      .map((prefix) => prefix.trim())
      .filter(Boolean)
    const saved = await saveSettingsPatch({ exported_lans: exportedLans })
    if (saved) setExportedLansDraft(exportedLans.join('\n'))
  }

  const saveLocalSocksListen = async () => {
    const nextListen = localSocksDraft.trim()
    if (!nextListen || nextListen === settings.local_socks5_listen) {
      setLocalSocksDraft(settings.local_socks5_listen || defaultSettings.local_socks5_listen)
      return
    }
    const saved = await saveSettingsPatch({ local_socks5_listen: nextListen }, { syncLocalSocks: true })
    if (!saved) {
      setLocalSocksDraft(settings.local_socks5_listen || defaultSettings.local_socks5_listen)
    }
  }

  const installTunHelper = async () => {
    setLoading(true)
    try {
      await api.installTunHelper()
      setStatus(await api.getStatus())
    } catch (e) {
      alert(formatActionError('Install helper', e))
    } finally {
      setLoading(false)
    }
  }

  const clientUi = status.client_ui

  // A phone has no switch to read: its VPN service is the only way to reach
  // other apps' traffic, so native routing is on whenever the runtime is.
  const nativeRoutingEnabled = caps.nativeRoutingSwitch
    ? settings.desktop_network_mode === 'lan_routes_tun'
    : true

  const nativeRoutingDescription = !nativeRoutingEnabled
    ? 'Let every app on this computer reach the Tunnel directly, instead of only the ones pointed at the local proxy.'
    : clientUi?.native_routing.state === 'ready'
      ? 'Applied: this computer is routing the Tunnel natively.'
      : clientUi?.native_routing.state === 'applying'
        ? 'Applying…'
        : clientUi?.native_routing.state === 'needs_helper'
          ? 'Install the native helper to apply this.'
          : 'Enabled, but native routes are not currently applied.'

  // Tunnel First only decides which of two overlapping networks wins once
  // routes exist. It used to be the only switch here that could start the TUN,
  // so turning it off reported Native routing: Disabled.
  const tunnelFirstDescription = nativeRoutingEnabled
    ? 'When a network is reachable both here and through the Tunnel, use the one here.'
    : 'Turn on Native routing to use this. Your answer is kept.'

  const exportedLanReadiness = new Map(
    settings.exported_lan_statuses.map((exportedLan) => [exportedLan.prefix, exportedLan.ready]),
  )
  // Anything published that the owner did not type came from the switch above.
  // Listing it keeps the section honest about what this machine is sharing.
  const automaticExportedLans = settings.exported_lan_statuses
    .map((exportedLan) => exportedLan.prefix)
    .filter((prefix) => !settings.exported_lans.includes(prefix))
  const localIngressEnabled = settings.local_proxy_enabled ?? true
  const copyLogs = async () => {
    const [latest, nextStatus, nextProxyStatus] = await Promise.all([
      api.getLogs(1000),
      api.getStatus(),
      api.getProxyStatus(),
    ])
    setLogs(latest)
    setStatus(nextStatus)
    setProxyStatus(nextProxyStatus)
    await copyText(buildLogText(formatNativeStatus(nextStatus, nextProxyStatus, productInfo), latest))
  }

  const nativeStatusText = formatNativeStatus(status, proxyStatus, productInfo)
  const localSocksDirty = localSocksDraft.trim() !== settings.local_socks5_listen
  const localSocksApplyLabel = proxyStatus.running && localSocksDirty ? 'Apply & restart' : 'Apply'
  const localSocksHint = localSocksDirty
    ? proxyStatus.running
      ? 'Pending change. Applying restarts the local proxy immediately.'
      : 'Pending change. Applying saves it for the next connect.'
    : proxyStatus.running
      ? `Listening at ${proxyStatus.listen_addr}`
      : status.connected
        ? 'Local proxy is starting or unavailable.'
        : 'Saved address. Proxy starts after connect.'
  const copyClashConfig = async () => {
    try {
      const yaml = await api.getClashConfig()
      await copyText(yaml)
      setClashCopied(true)
      if (clashCopyTimerRef.current) window.clearTimeout(clashCopyTimerRef.current)
      clashCopyTimerRef.current = window.setTimeout(() => setClashCopied(false), 1500)
    } catch (e) {
      alert(`Copy Clash config failed: ${e}`)
    }
  }

  if (initializing) {
    return (
      <div className="max-w-md mx-auto flex items-center justify-center h-64">
        <RefreshCw className="w-8 h-8 animate-spin text-accent" />
      </div>
    )
  }

  const peerDirectory = clientUi?.peer_directory
  const normalizedPeerSearch = peerSearch.trim().toLowerCase()
  const filteredPeers = (peerDirectory?.peers || []).filter((peer) => {
    if (!normalizedPeerSearch) return true
    return [peer.peer_id, peer.overlay_cidr, ...peer.exports.map((entry) => entry.prefix)]
      .some((value) => value.toLowerCase().includes(normalizedPeerSearch))
  })
  const visiblePeers = normalizedPeerSearch || showAllPeers ? filteredPeers : filteredPeers.slice(0, 10)
  const hiddenPeerCount = Math.max(0, filteredPeers.length - visiblePeers.length)
  const selectedPeerProfile = peerProfiles.find((profile) => profile.tunnel_id === selectedTunnelId)

  return (
    <div className="max-w-md mx-auto">
      <div className="mb-4">
        <div>
          <h1 className="text-xl font-bold">{productInfo.display_name}</h1>
          <p className="text-sm text-content-muted">
            Peer · v{productInfo.version}
          </p>
        </div>
      </div>

      <TabBar activeTab={activeTab} onChange={setActiveTab} />

      {activeTab === 'connection' && (
        <div className="space-y-3">
          <div className={`glass rounded-xl p-4 ${overallBorderClass(clientUi?.overall)}`}>
            <div className="flex items-center gap-4 mb-4">
              <div
                className={`w-14 h-14 rounded-full flex items-center justify-center ${
                  clientUi?.overall === 'connected'
                    ? 'bg-status-success/10 text-status-success'
                    : clientUi?.overall === 'starting' || clientUi?.overall === 'waiting_for_gateway'
                    ? 'bg-status-warning/10 text-status-warning'
                    : 'bg-surface-subdued text-content-muted'
                }`}
              >
                {clientUi?.overall === 'connected' ? <Wifi className="w-7 h-7" /> : clientUi?.overall === 'starting' || clientUi?.overall === 'waiting_for_gateway' ? <RefreshCw className="w-7 h-7 animate-spin" /> : <WifiOff className="w-7 h-7" />}
              </div>
              <div className="min-w-0">
                <div className="text-lg font-semibold">{overallStateLabel(clientUi?.overall)}</div>
                <div className="text-sm text-content-muted truncate">
                  Gateway attachment: {gatewayAttachmentLabel(clientUi?.gateway_attachment)}
                </div>
                {clientUi?.overall_reason_code && (
                  <div className={`text-xs ${clientUi.overall === 'degraded' || clientUi.overall === 'blocked' ? 'text-status-warning' : 'text-content-muted'}`}>
                    {reasonText(clientUi.overall_reason_code)}
                  </div>
                )}
              </div>
            </div>

            <div className="space-y-2 mt-3 pt-3 border-t border-border">
              <div>
                <div className="flex items-baseline gap-2">
                  <span className="text-xs uppercase tracking-wide text-accent">This Peer</span>
                  {clientUi?.this_peer ? (
                    <span className="font-mono text-sm">{clientUi.this_peer.overlay_cidr}</span>
                  ) : (
                    <span className="text-sm text-content-muted">Identity unavailable from the current runtime</span>
                  )}
                </div>
              </div>
              <div className="flex flex-wrap gap-2">
                <StatusBadge label="Mesh" value={meshStateLabel(clientUi?.mesh.state)} tone={runtimeStateTone(clientUi?.mesh.state)} />
                <StatusBadge label="Native routing" value={nativeRoutingStateLabel(clientUi?.native_routing.state)} tone={runtimeStateTone(clientUi?.native_routing.state)} />
              </div>
              <StatusRow label="Uptime" value={formatUptime(status.uptime_secs)} />
            </div>
          </div>


          <div className="glass rounded-xl p-4 space-y-2">
            <h2 className="text-base font-semibold text-content-secondary">Traffic</h2>
            <StatusRow
              label="Direct"
              value={clientUi ? trafficLabel(clientUi.traffic.direct_tx_bytes, clientUi.traffic.direct_rx_bytes) : 'Unavailable'}
            />
            <StatusRow
              label="Relay"
              value={clientUi ? trafficLabel(clientUi.traffic.relay_tx_bytes, clientUi.traffic.relay_rx_bytes) : 'Unavailable'}
            />
            {clientUi?.relay_usage && (
              <StatusRow
                label="Relay this month"
                value={`${formatBytes(clientUi.relay_usage.used_bytes)} of ${formatBytes(clientUi.relay_usage.allowance_bytes)}`}
              />
            )}
          </div>

          <div className="glass rounded-xl p-4">
            <div className="space-y-3 mb-4">
              <div className="flex items-center justify-between gap-3">
                <div className="text-sm text-content-muted">
                  {peerProfiles.length === 0 ? 'Import a Peer profile to join a Tunnel.' : `${peerProfiles.length} imported profile${peerProfiles.length === 1 ? '' : 's'}`}
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <button
                    type="button"
                    onClick={importPeerProfile}
                    disabled={loading || status.connected || status.connecting}
                    className="shrink-0 border border-accent/30 bg-accent-soft text-accent hover:bg-accent/15"
                  >
                    Import .peer
                  </button>
                  {caps.qrScanner && (
                    <button
                      type="button"
                      aria-label="Scan a Peer profile QR code"
                      title="Scan a Peer profile QR code"
                      onClick={scanPeerProfile}
                      disabled={loading || status.connected || status.connecting}
                      className="shrink-0 border border-border bg-surface-subdued !px-3 !py-3"
                    >
                      <QrCode className="w-4 h-4 text-content-muted" />
                    </button>
                  )}
                </div>
              </div>
              {peerProfiles.length > 0 && (
                <Field label="Peer profile">
                  <div className="flex items-center gap-2">
                    <select
                      value={selectedTunnelId}
                      onChange={(event) => setSelectedTunnelId(event.target.value)}
                      disabled={status.connected || status.connecting || loading}
                      className="min-w-0 flex-1"
                    >
                      {peerProfiles.map((profile) => (
                        <option key={profile.tunnel_id} value={profile.tunnel_id}>
                          {profile.overlay_ip}
                        </option>
                      ))}
                    </select>
                    {selectedPeerProfile && (
                      <button
                        type="button"
                        aria-label="Remove this profile"
                        title="Remove this profile"
                        onClick={() => void forgetProfile(selectedPeerProfile.tunnel_id)}
                        disabled={loading || status.connected || status.connecting}
                        className="shrink-0 border border-border bg-surface-subdued !px-3 !py-3"
                      >
                        <Trash2 className="w-4 h-4 text-content-muted" />
                      </button>
                    )}
                  </div>
                </Field>
              )}
              {selectedPeerProfile?.bootstrap_kind === 'managed_platform' && (
                <p className="text-xs text-content-muted">Gateway facts are resolved through the managed Platform.</p>
              )}
            </div>

            {status.connected || status.connecting ? (
              <button onClick={handleDisconnect} disabled={loading} className="w-full bg-status-danger hover:bg-status-danger text-content-inverse border-none">
                {loading ? <RefreshCw className="w-5 h-5 animate-spin mx-auto" /> : status.connecting ? 'Cancel' : 'Disconnect'}
              </button>
            ) : (
              <button onClick={handleConnect} disabled={loading || !selectedTunnelId} className="w-full bg-accent hover:bg-focus text-content-inverse border-none shadow-sm">
                {loading ? <RefreshCw className="w-5 h-5 animate-spin mx-auto" /> : 'Connect'}
              </button>
            )}
          </div>
        </div>
      )}

      {activeTab === 'peers' && (
        <div className="space-y-3">
          {clientUi?.this_peer && (
            <div className="glass rounded-xl p-3">
              <div className="flex items-baseline gap-2">
                <span className="text-xs uppercase tracking-wide text-content-muted">This device</span>
                <span className="font-mono text-sm font-medium">{clientUi.this_peer.overlay_cidr}</span>
              </div>
            </div>
          )}
          <section className="space-y-3">
            <div className="flex items-center justify-between gap-3">
              <h2 className="text-base font-semibold">
                Peers in this tunnel{peerDirectory?.state === 'ready' ? ` (${peerDirectory.peers.length})` : ''}
              </h2>
            </div>
            {peerDirectory?.state !== 'unavailable' && (
              <label className="relative block">
                <Search className="absolute left-3 top-3.5 w-4 h-4 text-content-muted" />
                <input
                  className="!pl-10"
                  value={peerSearch}
                  onChange={(event) => setPeerSearch(event.target.value)}
                  placeholder="Search by address or network"
                  aria-label="Search the other devices"
                />
              </label>
            )}
            {peerDirectory?.state === 'unavailable' || !peerDirectory ? (
              <div className="glass rounded-xl p-3 text-sm text-content-muted">
                {peerDirectory?.reason_code ? reasonText(peerDirectory.reason_code) : 'Waiting for the runtime.'}
              </div>
            ) : visiblePeers.length === 0 ? (
              <div className="glass rounded-xl p-3 text-sm text-content-muted">
                {normalizedPeerSearch ? 'No known Peer matches this search.' : peerDirectory.state === 'syncing' ? 'Syncing known remote Peers…' : 'No known remote Peers.'}
              </div>
            ) : (
              visiblePeers.map((peer) => <PeerRow key={peer.peer_id} peer={peer} />)
            )}
            {hiddenPeerCount > 0 && (
              <button
                type="button"
                onClick={() => setShowAllPeers(true)}
                className="w-full !py-2 border border-border bg-surface-subdued text-content-secondary"
              >
                Show {hiddenPeerCount} more <ChevronDown className="inline w-4 h-4 ml-1" />
              </button>
            )}
          </section>
        </div>
      )}

      {activeTab === 'settings' && (
        <div className="space-y-3 mb-4">
          <section className="glass rounded-xl p-4 space-y-2">
            <h3 className="font-semibold text-accent">Connection</h3>
            {caps.startAtLogin && (
              <Toggle label="Start at login" desc="Starts the Client when you log in." checked={settings.auto_start} onChange={(v) => saveSettingsPatch({ auto_start: v })} />
            )}
            <Toggle label="Auto-connect" desc="Reconnects the Peer profile selected last time. There are no stored credentials." checked={settings.auto_connect} onChange={(v) => saveSettingsPatch({ auto_connect: v })} />
          </section>

          <section className="glass rounded-xl p-4 space-y-3">
            <h3 className="font-semibold text-accent">Network</h3>
            {caps.nativeRoutingSwitch && (
              <Toggle
                label="Native routing"
                desc={nativeRoutingDescription}
                checked={nativeRoutingEnabled}
                disabled={status.connecting || loading}
                onChange={(enabled) =>
                  saveSettingsPatch({ desktop_network_mode: enabled ? 'lan_routes_tun' : 'socks5_only' })
                }
              />
            )}
            <Toggle
              label="Tunnel First"
              desc={tunnelFirstDescription}
              checked={settings.tunnel_first}
              disabled={!nativeRoutingEnabled || status.connecting || loading}
              onChange={(enabled) => saveSettingsPatch({ tunnel_first: enabled })}
            />
            <Toggle
              label="LAN P2P"
              desc="Reach devices on the same network without going through a Relay. Off by default: it tells the other devices this one's local addresses."
              checked={settings.p2p_allow_lan_candidates ?? false}
              disabled={status.connected || status.connecting || loading}
              onChange={(enabled) => saveSettingsPatch({ p2p_allow_lan_candidates: enabled })}
            />
            {!caps.nativeRoutingSwitch && (
              <StatusRow
                label="Native routing"
                value={nativeRoutingStateLabel(clientUi?.native_routing.state)}
                tone={runtimeStateTone(clientUi?.native_routing.state)}
              />
            )}
            <Toggle
              label="Export Current LAN"
              desc="Share the network this computer is on with the rest of your Tunnel, without naming it. It follows this computer between networks, and leaves the list below alone."
              checked={settings.auto_export_current_lan ?? true}
              disabled={loading}
              onChange={(enabled) => saveSettingsPatch({ auto_export_current_lan: enabled })}
            />
            <Field label={<>LAN Export<InfoHint text="One private network per line, e.g. 192.168.1.0/24. Other devices in the Tunnel reach them through this one. The whole update is rejected if any line is invalid or repeated." /></>}>
              <textarea
                rows={4}
                value={exportedLansDraft}
                onChange={(event) => setExportedLansDraft(event.target.value)}
                placeholder={'192.168.1.0/24\n10.20.0.0/16'}
                disabled={loading}
              />
            </Field>

            <button
              type="button"
              onClick={saveExportedLans}
              disabled={loading}
              className="w-full bg-accent-soft hover:bg-accent/15 border border-accent/30 text-accent"
            >
              Save Exported LANs
            </button>
            {settings.exported_lans.length === 0 && automaticExportedLans.length === 0 ? (
              <div className="text-xs text-content-muted">No LAN prefixes configured.</div>
            ) : !caps.exportReadiness ? null : (
              <>
                {settings.exported_lans.map((prefix) => {
                  // An idle Client has reported no interface facts yet, which is
                  // not the same as an interface that is missing. Warning-toning
                  // every configured prefix while disconnected said the setup was
                  // broken when nothing had been asked of it.
                  const known = exportedLanReadiness.has(prefix)
                  const ready = exportedLanReadiness.get(prefix) === true
                  return (
                    <div key={prefix} className="flex items-center justify-between gap-3 text-sm">
                      <span className="font-mono break-all">{prefix}</span>
                      <span className={`shrink-0 ${!known ? 'text-content-muted' : ready ? 'text-status-success' : 'text-status-warning'}`}>
                        {!known ? 'Checked once connected' : ready ? 'Published' : 'Interface unavailable'}
                      </span>
                    </div>
                  )
                })}
                {automaticExportedLans.map((prefix) => (
                  <div key={prefix} className="flex items-center justify-between gap-3 text-sm">
                    <span className="font-mono break-all">{prefix}</span>
                    <span className="shrink-0 text-status-success">Published · this network</span>
                  </div>
                ))}
              </>
            )}
            {clientUi?.native_routing.actions.includes('install_helper') && (
              <button
                type="button"
                onClick={installTunHelper}
                disabled={loading || status.connecting}
                className="w-full bg-accent-soft hover:bg-accent/15 border border-accent/30 text-accent disabled:opacity-40 disabled:cursor-not-allowed"
              >
                Install Helper
              </button>
            )}

            {caps.localProxy && (
              <>
              <Toggle
                label="Local proxy"
                desc="Opens a SOCKS5 port on 127.0.0.1 for browsers and other apps on this computer. Loopback only — never opened to the network, and no password. Turning it off does not affect what Peers can reach."
                checked={localIngressEnabled}
                disabled={status.connecting || loading}
                onChange={(v) => saveSettingsPatch({ local_proxy_enabled: v })}
              />
              {localIngressEnabled && (
                <>
                  <div className="mt-2">
                    <div className="flex gap-2">
                      <input
                        type="text"
                        value={localSocksDraft}
                        onChange={(e) => setLocalSocksDraft(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') {
                            e.preventDefault()
                            saveLocalSocksListen()
                            e.currentTarget.blur()
                          }
                          if (e.key === 'Escape') {
                            setLocalSocksDraft(settings.local_socks5_listen || defaultSettings.local_socks5_listen)
                            e.currentTarget.blur()
                          }
                        }}
                        placeholder="127.0.0.1:1080"
                        disabled={status.connecting || loading}
                      />
                      <button
                        type="button"
                        onMouseDown={(e) => e.preventDefault()}
                        onClick={saveLocalSocksListen}
                        disabled={!localSocksDirty || status.connecting || loading}
                        title="Apply local SOCKS5 address"
                        className="shrink-0 min-w-[7.5rem] !px-4 !py-3 bg-accent-soft hover:bg-accent/15 border border-accent/30 text-accent"
                      >
                        {localSocksDirty ? localSocksApplyLabel : <Check className="w-5 h-5 mx-auto" />}
                      </button>
                    </div>
                    <div className="mt-2 flex items-start justify-between gap-3 text-xs">
                      <span className={proxyStatus.running && !localSocksDirty ? 'text-status-success' : 'text-content-muted'}>
                        {localSocksHint}
                      </span>
                      {localSocksDirty && (
                        <button
                          type="button"
                          onClick={() => setLocalSocksDraft(settings.local_socks5_listen || defaultSettings.local_socks5_listen)}
                          className="shrink-0 border-none bg-transparent !p-0 text-xs text-accent hover:text-accent"
                        >
                          Revert
                        </button>
                      )}
                      {!localSocksDirty && proxyStatus.running && (
                        <button
                          type="button"
                          onClick={copyClashConfig}
                          disabled={!status.connected}
                          className="shrink-0 border-none bg-transparent !p-0 text-xs text-accent hover:text-accent disabled:opacity-40 disabled:cursor-not-allowed"
                        >
                          {clashCopied ? 'Copied' : 'Copy Clash'}
                        </button>
                      )}
                    </div>
                  </div>
                </>
              )}
              </>
            )}
          </section>

          <section className="glass rounded-xl p-4 space-y-3">
            <h3 className="font-semibold text-accent">Access<InfoHint text="Who may reach what this device shares. No rules means every Peer in the Tunnel may." /></h3>

            {settings.v2_settings_rejected && (
              <p className="text-sm text-danger border border-danger/30 bg-danger/5 rounded-lg p-2">
                These settings could not be read, so this device is refusing everything
                until they are fixed. What is shown below is what was saved, not what is
                running.
              </p>
            )}
            <p className="text-sm text-content-secondary">{accessSummary(allowRules, denyRules)}</p>

            <Toggle
              label="Block all"
              desc="Refuse every destination, including ones an Allow rule names"
              checked={isClosedPolicy(denyRules)}
              disabled={loading}
              onChange={(blocked) =>
                saveClientAccess(
                  allowRules,
                  blocked
                    ? [...denyRules.filter((rule) => !isCatchAllRule(rule)), ...CLOSED_DENY_RULES]
                    : denyRules.filter((rule) => !isCatchAllRule(rule)),
                )
              }
            />

            {isClosedPolicy(denyRules) && (
              <p className="text-xs text-content-secondary">
                Rules below are kept and take effect again when this is switched off.
              </p>
            )}
              <>
                <RuleList
                  title="Allowed destinations"
                  empty="No rules — the summary above says what that means."
                  rules={allowRules}
                  disabled={loading}
                  onRemove={(index) => saveClientAccess(allowRules.filter((_, i) => i !== index), denyRules)}
                />
                <RuleList
                  title="Blocked destinations"
                  empty="No rules."
                  rules={denyRules}
                  disabled={loading}
                  onRemove={(index) => saveClientAccess(allowRules, denyRules.filter((_, i) => i !== index))}
                />
                <RuleForm
                  disabled={loading}
                  onAdd={(list, rule) =>
                    list === 'allow'
                      ? saveClientAccess([...allowRules, rule], denyRules)
                      : saveClientAccess(allowRules, [...denyRules, rule])
                  }
                />
              </>

          </section>

        </div>
      )}

      {activeTab === 'logs' && (
        <div className="glass rounded-xl p-3">
          <div className="flex items-center justify-between mb-3">
            <div className="flex items-center gap-2">
              <FileText className="w-5 h-5 text-accent" />
              <h3 className="font-medium">Logs ({logs.length})</h3>
            </div>
            <div className="flex items-center gap-2">
              <button
                onClick={copyLogs}
                className="p-1.5 rounded-lg hover:bg-surface-subdued transition"
                title="Copy Logs"
              >
                <Copy className="w-4 h-4 text-content-muted" />
              </button>
              <select
                value={settings.log_level || 'info'}
                onChange={(e) => saveSettingsPatch({ log_level: e.target.value })}
                className="w-24 py-1.5 px-2 text-xs"
              >
                <option value="error">Error</option>
                <option value="warn">Warn</option>
                <option value="info">Info</option>
                <option value="debug">Debug</option>
                <option value="trace">Trace</option>
              </select>
              <button
                onClick={() => api.clearLogs().then(() => setLogs([]))}
                className="p-1.5 rounded-lg hover:bg-surface-subdued transition"
                title="Clear Logs"
              >
                <Trash2 className="w-4 h-4 text-content-muted" />
              </button>
            </div>
          </div>
          <div
            ref={logContainerRef}
            className="bg-surface-subdued rounded-lg p-3 h-64 overflow-y-auto font-mono text-xs text-content-secondary"
          >
            <div className="text-content-secondary whitespace-pre-wrap break-words">
              <div className="text-content-muted">Native status:</div>
              <div>{nativeStatusText}</div>
            </div>
            <div className="my-3 border-t border-border" />
            {logs.length === 0 && <div className="text-content-muted text-center py-8">No logs yet</div>}
            {logs.map((line, i) => (
              <div key={i} className="py-0.5 text-content-secondary whitespace-pre-wrap break-words">
                {line}
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

/**
 * The Deny rules that refuse every address on both protocols and families.
 *
 * "Block everything" has to be spelled out, because an empty Allow list means
 * open. Keeping the exact shape here means what the toggle writes is what the
 * Rust side recognises as closed.
 */
const CLOSED_DENY_RULES: ClientAccessRuleV2[] = ['0.0.0.0/0', '::/0'].flatMap((cidr) =>
  (['tcp', 'udp'] as const).map((protocol) => ({
    target: { type: 'cidr' as const, value: cidr },
    protocol,
    port: { type: 'any' as const },
  })),
)

/**
 * Whether a rule is one the "block everything" switch wrote.
 *
 * The switch owns exactly the four rules `CLOSED_DENY_RULES` names. Matching
 * more loosely than that let it delete a Deny the owner had written by hand,
 * and let a hand-written pair flip the switch on and hide the editor.
 */
function isCatchAllRule(rule: ClientAccessRuleV2): boolean {
  return CLOSED_DENY_RULES.some(
    (owned) =>
      owned.target.type === rule.target.type &&
      (owned.target as { value?: string }).value === (rule.target as { value?: string }).value &&
      owned.protocol === rule.protocol &&
      owned.port.type === rule.port.type,
  )
}

// Both families, per protocol. Accepting either meant a policy that denied only
// the IPv4 catch-all printed "Nothing is reachable through this device" while
// every IPv6 destination still was.
function isClosedPolicy(deny: ClientAccessRuleV2[]): boolean {
  return (['tcp', 'udp'] as const).every((protocol) =>
    (['0.0.0.0/0', '::/0'] as const).every((family) =>
      deny.some(
        (rule) =>
          rule.protocol === protocol &&
          rule.port.type === 'any' &&
          rule.target.type === 'cidr' &&
          rule.target.value === family,
      ),
    ),
  )
}

/**
 * One sentence saying what the rules add up to.
 *
 * The rules match the destination a Peer asked for, not the Peer that asked —
 * the policy is target-side and never selects a source. Wording this as "who
 * can reach me" was backwards, and would have had someone add their laptop's
 * address expecting a guest list.
 */
function accessSummary(allow: ClientAccessRuleV2[], deny: ClientAccessRuleV2[]): string {
  if (isClosedPolicy(deny)) return 'Nothing is reachable through this device.'
  // A this-device mapping exposes a local port; it is a capability, not a gate,
  // so it does not turn the Allow list into a whitelist.
  const gates = allow.filter((rule) => rule.target.type !== 'this_peer')
  if (gates.length === 0) {
    return deny.length === 0
      ? 'Peers can reach anything this device publishes.'
      : 'Peers can reach anything this device publishes, except what is blocked below.'
  }
  return 'Peers can reach only the destinations listed under Allowed. Blocked rules still win.'
}

/** Enough of a Peer ID to tell two devices apart without a wall of UUID. */
/**
 * Turns a runtime reason code into something worth reading.
 *
 * These are snake_case identifiers meant for logs. Rendering one verbatim, in
 * red, on the first screen told a freshly installed Client it was broken when
 * it had simply never been connected.
 */
const REASON_TEXT: Record<string, string> = {
  client_access_runtime_consumer_unavailable: 'Access rules load once connected.',
  connecting_to_gateway: 'Connecting to a Gateway\u2026',
  gateway_authentication_rejected: 'The Gateway refused this device.',
  gateway_connect_failed: 'Could not reach the Gateway.',
  gateway_tls_failed: 'The Gateway\u2019s certificate was not accepted.',
  gateway_unavailable: 'No Gateway is available right now.',
  gateway_unavailable_direct_preserved: 'No Gateway right now; direct connections still work.',
  initial_full_sync_pending: 'Getting the first update\u2026',
  lan_export_runtime_consumer_unavailable: 'Published networks load once connected.',
  membership_cycle_pending: 'Waiting for the Tunnel\u2019s device list\u2026',
  native_apply_failed: 'The system network settings could not be applied.',
  native_apply_in_progress: 'Applying system network settings\u2026',
  native_apply_permission_denied: 'Permission to change network settings was refused.',
  native_apply_result_unavailable: 'The system did not report whether the change applied.',
  native_helper_not_installed: 'The network helper is not installed yet.',
  no_eligible_gateway: 'No Gateway can serve this Tunnel right now.',
  no_usable_peer_path: 'No way to reach this device yet.',
  peer_link_unavailable: 'This device cannot be reached right now.',
  platform_unavailable: 'Cannot reach Lantunnel right now.',
  resolving_through_platform: 'Finding a Gateway\u2026',
  runtime_failed: 'The connection stopped unexpectedly.',
  runtime_inactive: 'Not connected yet.',
  runtime_snapshot_unavailable: 'Waiting for the first status\u2026',
  scope_rejected: 'The Gateway rejected this Tunnel\u2019s configuration.',
  tunnel_first_runtime_consumer_unavailable: 'This setting loads once connected.',
}

const GATEWAY_PHASE_TEXT: Record<string, string> = {
  unknown: 'Not known yet',
  resolving_through_platform: 'Finding a Gateway',
  connecting: 'Connecting',
  attached: 'Attached',
  provisioning_scope: 'Setting up',
  rejected: 'Configuration refused',
  tls_failed: 'Certificate not accepted',
  unavailable: 'Unavailable',
}

function reasonText(code: string): string {
  return REASON_TEXT[code] ?? code.replace(/_/g, ' ').replace(/^./, (c) => c.toUpperCase())
}

function describeRule(rule: ClientAccessRuleV2): string {
  const where =
    rule.target.type === 'this_peer' ? 'this device' : rule.target.value
  const port = rule.port.type === 'any' ? 'any port' : `port ${rule.port.value}`
  return `${rule.protocol.toUpperCase()} · ${where} · ${port}`
}

function RuleList(props: {
  title: string
  empty: string
  rules: ClientAccessRuleV2[]
  disabled: boolean
  onRemove: (index: number) => void
}) {
  return (
    <div className="space-y-1.5">
      <div className="text-xs font-medium uppercase tracking-wide text-content-muted">{props.title}</div>
      {props.rules.length === 0 ? (
        <p className="text-sm text-content-secondary">{props.empty}</p>
      ) : (
        props.rules.map((rule, index) => (
          <div
            key={`${props.title}-${index}`}
            className="flex items-center justify-between gap-2 rounded-lg border border-border bg-surface-subdued px-3 py-2"
          >
            <span className="text-sm break-all">{describeRule(rule)}</span>
            <button
              type="button"
              onClick={() => props.onRemove(index)}
              disabled={props.disabled}
              aria-label={`Remove ${describeRule(rule)}`}
              className="shrink-0 px-2 py-1 text-xs bg-surface border border-border text-content-secondary"
            >
              Remove
            </button>
          </div>
        ))
      )}
    </div>
  )
}

function RuleForm(props: {
  disabled: boolean
  onAdd: (list: 'allow' | 'deny', rule: ClientAccessRuleV2) => Promise<boolean>
}) {
  const [list, setList] = useState<'allow' | 'deny'>('allow')
  const [targetType, setTargetType] = useState<'this_peer' | 'ip' | 'cidr' | 'host'>('cidr')
  const [targetValue, setTargetValue] = useState('')
  const [protocol, setProtocol] = useState<'tcp' | 'udp'>('tcp')
  const [port, setPort] = useState('')

  const submit = () => {
    if (targetType !== 'this_peer' && !targetValue.trim()) {
      alert('Enter an address, network or hostname.')
      return
    }
    const portNumber = Number(port)
    if (port.trim() && (!Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535)) {
      alert('Port must be between 1 and 65535, or left blank for any port.')
      return
    }
    // A this-device rule is the only way to expose a local port, and it needs a
    // port to expose; "any port" would mean the whole machine.
    if (targetType === 'this_peer' && !port.trim()) {
      alert('A rule for this device needs the port other Peers will ask for.')
      return
    }
    void props
      .onAdd(list, {
        target:
          targetType === 'this_peer'
            ? { type: 'this_peer' }
            : { type: targetType, value: targetValue.trim() },
        protocol,
        port: port.trim() ? { type: 'exact', value: portNumber } : { type: 'any' },
      })
      .then((saved) => {
        // Clearing before the save is known to have worked loses what was
        // typed, and the rule that failed is exactly the one worth keeping.
        if (!saved) return
        setTargetValue('')
        setPort('')
      })
  }

  return (
    <div className="space-y-2 rounded-lg border border-border p-3">
      <div className="text-xs font-medium uppercase tracking-wide text-content-muted">Add a rule</div>
      <p className="text-xs text-content-secondary">
        A rule names a destination a Peer may ask for — an address behind this device, or a port on it.
        It does not name which Peer is asking.
      </p>
      <div className="grid grid-cols-2 gap-2">
        <select aria-label="Allow or block" value={list} onChange={(e) => setList(e.target.value as 'allow' | 'deny')} disabled={props.disabled}>
          <option value="allow">Allow</option>
          <option value="deny">Block</option>
        </select>
        <select aria-label="Protocol" value={protocol} onChange={(e) => setProtocol(e.target.value as 'tcp' | 'udp')} disabled={props.disabled}>
          <option value="tcp">TCP</option>
          <option value="udp">UDP</option>
        </select>
      </div>
      <select
        aria-label="What the rule names"
        value={targetType}
        onChange={(e) => setTargetType(e.target.value as typeof targetType)}
        disabled={props.disabled}
      >
        <option value="cidr">A network range</option>
        <option value="ip">A single address</option>
        <option value="host">A hostname</option>
        <option value="this_peer">A service on this device</option>
      </select>
      {targetType !== 'this_peer' && (
        <input
          aria-label="Address, network or hostname"
          value={targetValue}
          onChange={(e) => setTargetValue(e.target.value)}
          placeholder={targetType === 'cidr' ? '192.168.1.0/24' : targetType === 'ip' ? '10.0.0.9' : 'printer.local'}
          disabled={props.disabled}
        />
      )}
      <input
        aria-label="Port"
        value={port}
        onChange={(e) => setPort(e.target.value)}
        placeholder={targetType === 'this_peer' ? 'Port other Peers ask for, e.g. 8080' : 'Port, or blank for any'}
        inputMode="numeric"
        disabled={props.disabled}
      />
      <button
        type="button"
        onClick={submit}
        disabled={props.disabled}
        className="w-full bg-accent-soft hover:bg-accent/15 border border-accent/30 text-accent"
      >
        Add rule
      </button>
    </div>
  )
}

function parseClientAccessPolicy(draft: string): { allow: ClientAccessRuleV2[]; deny: ClientAccessRuleV2[] } {
  let parsed: unknown
  try {
    parsed = JSON.parse(draft)
  } catch {
    return { allow: parseClientAccessRuleList(draft, 'Policy'), deny: [] }
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('Expected an object with allow and deny lists.')
  }
  const record = parsed as Record<string, unknown>
  return {
    allow: parseClientAccessRuleList(JSON.stringify(record.allow ?? []), 'Allow'),
    deny: parseClientAccessRuleList(JSON.stringify(record.deny ?? []), 'Deny'),
  }
}

function parseClientAccessRuleList(draft: string, label: string): ClientAccessRuleV2[] {
  let parsed: unknown
  try {
    parsed = JSON.parse(draft)
  } catch (error) {
    throw new Error(`${label} rules must be valid JSON: ${errorMessage(error)}`)
  }
  if (!Array.isArray(parsed)) {
    throw new Error(`${label} rules must be a JSON array`)
  }
  return parsed as ClientAccessRuleV2[]
}

async function copyText(text: string): Promise<void> {
  let nativeError: unknown
  try {
    await api.writeClipboardText(text)
    return
  } catch (e) {
    nativeError = e
  }

  try {
    await writeTextWithWebClipboard(text)
  } catch {
    throw nativeError
  }
}

async function writeTextWithWebClipboard(text: string): Promise<void> {
  let clipboardError: unknown
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text)
      return
    } catch (e) {
      clipboardError = e
    }
  }

  if (copyTextWithSelectionFallback(text)) return
  throw clipboardError ?? new Error('clipboard is not available')
}

function copyTextWithSelectionFallback(text: string): boolean {
  const textarea = document.createElement('textarea')
  const activeElement = document.activeElement instanceof HTMLElement ? document.activeElement : null
  textarea.value = text
  textarea.setAttribute('readonly', '')
  textarea.style.position = 'fixed'
  textarea.style.left = '-9999px'
  textarea.style.top = '0'
  document.body.appendChild(textarea)
  textarea.select()
  textarea.setSelectionRange(0, textarea.value.length)
  try {
    return document.execCommand('copy')
  } finally {
    document.body.removeChild(textarea)
    activeElement?.focus()
  }
}


function formatActionError(action: string, error: unknown): string {
  const detail = errorMessage(error)
  if (isDesktopTunPermissionError(detail)) {
    return `${action} failed: tunnel routes via TUN needs administrator approval.\n\n${desktopTunPermissionResolution()}\n\nTechnical detail: ${briefErrorDetail(detail)}`
  }
  return `${action} failed: ${detail}`
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  return String(error)
}

function isDesktopTunPermissionError(detail: string): boolean {
  const normalized = detail.toLowerCase()
  return (
    normalized.includes('tun_permission_required') ||
    (
      normalized.includes('operation not permitted') &&
      (normalized.includes('utun') || normalized.includes('route changes') || normalized.includes('socks5 tunnel open'))
    )
  )
}

function desktopTunPermissionResolution(): string {
  const platform = navigator.platform.toLowerCase()
  if (platform.includes('mac')) {
    return 'macOS does not have a System Settings switch for this. Turn off tunnel routes via TUN and use Local SOCKS5, or, for testing, launch the app with administrator privileges. A packaged privileged helper is required for normal one-click TUN mode.'
  }
  if (platform.includes('win')) {
    return 'Run Lantunnel as administrator and make sure the Wintun driver is installed, or turn off tunnel routes via TUN and use Local SOCKS5.'
  }
  return 'Run Lantunnel with root/CAP_NET_ADMIN, or turn off tunnel routes via TUN and use Local SOCKS5.'
}

function briefErrorDetail(detail: string): string {
  return detail
    .replace(/^TUN_PERMISSION_REQUIRED:\s*/i, '')
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, 240)
}

function TabBar(props: { activeTab: AppTab; onChange: (tab: AppTab) => void }) {
  return (
    <div className="flex gap-1 rounded-xl border border-border bg-surface-subdued p-1 mb-6" role="tablist" aria-label="Desktop navigation">
      <TabButton label="Connection" tab="connection" activeTab={props.activeTab} onChange={props.onChange} />
      <TabButton label="Peers" tab="peers" activeTab={props.activeTab} onChange={props.onChange} />
      <TabButton label="Settings" tab="settings" activeTab={props.activeTab} onChange={props.onChange} />
      <TabButton label="Logs" tab="logs" activeTab={props.activeTab} onChange={props.onChange} />
    </div>
  )
}

function TabButton(props: { label: string; tab: AppTab; activeTab: AppTab; onChange: (tab: AppTab) => void }) {
  const selected = props.tab === props.activeTab
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      onClick={() => props.onChange(props.tab)}
      className={`flex-1 !px-3 !py-2 rounded-lg text-sm font-semibold transition ${
        selected ? 'bg-accent text-content-inverse shadow-sm' : 'text-content-secondary hover:bg-surface'
      }`}
    >
      {props.label}
    </button>
  )
}

function Field(props: { label: React.ReactNode; children: React.ReactNode }) {
  return (
    <div>
      <label className="block text-sm font-medium text-content-muted mb-2">{props.label}</label>
      {props.children}
    </div>
  )
}

function StatusRow(props: { label: string; value: string; tone?: 'green' | 'yellow' | 'slate' }) {
  const toneClass =
    props.tone === 'green' ? 'text-status-success' : props.tone === 'yellow' ? 'text-status-warning' : 'text-content-secondary'
  return (
    <div className="flex items-center gap-2 text-sm min-w-0">
      <span className="text-content-muted shrink-0">{props.label}:</span>
      <span className={`${toneClass} min-w-0 truncate`}>{props.value}</span>
    </div>
  )
}

function StatusBadge(props: { label: string; value: string; tone: 'green' | 'yellow' | 'slate' }) {
  const toneClass =
    props.tone === 'green'
      ? 'border-status-success/40 text-status-success'
      : props.tone === 'yellow'
        ? 'border-status-warning/40 text-status-warning'
        : 'border-border text-content-secondary'
  return (
    <span className={`rounded-full border px-3 py-1 text-xs ${toneClass}`}>
      {props.label}: {props.value}
    </span>
  )
}

function PeerRow({ peer }: { peer: RemotePeerRowV2 }) {
  return (
    <article className="glass rounded-xl p-3 space-y-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="font-mono text-sm font-medium">{peer.overlay_cidr}</div>
        </div>
        {peer.current_path && (
          <span className={`shrink-0 rounded-full border px-3 py-1 text-xs ${peer.current_path === 'direct' ? 'border-status-success/40 text-status-success' : 'border-accent/50 text-accent'}`}>
            {peer.current_path === 'direct' ? 'Direct' : 'Encrypted Relay'}
          </span>
        )}
      </div>
      <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-content-muted">
        <span>{remotePeerStateLabel(peer.state)}</span>
        <span>{routingStateLabel(peer.routing)}</span>

      </div>
      {peer.reason_code && <div className="text-xs text-status-warning">{reasonText(peer.reason_code)}</div>}
      <div className="space-y-1 text-sm">
        {peer.exports.length === 0 ? (
          <div className="text-content-muted">No exported LANs</div>
        ) : (
          peer.exports.map((entry) => (
            <div key={entry.prefix} className="flex flex-wrap justify-between gap-2">
              <span>{entry.prefix}</span>
              <span className="text-content-muted">{exportPlacementLabel(entry.placement)}</span>
            </div>
          ))
        )}
      </div>
    </article>
  )
}

function overallBorderClass(state: NonNullable<ConnectionStatus['client_ui']>['overall'] | undefined) {
  if (state === 'connected') return 'border-status-success/40'
  if (state === 'starting' || state === 'waiting_for_gateway' || state === 'degraded') return 'border-status-warning/40'
  if (state === 'blocked') return 'border-status-danger/40'
  return ''
}

function overallStateLabel(state: NonNullable<ConnectionStatus['client_ui']>['overall'] | undefined) {
  switch (state) {
    case 'connected': return 'Connected'
    case 'starting': return 'Starting'
    case 'waiting_for_gateway': return 'Waiting for Gateway'
    case 'degraded': return 'Degraded'
    case 'blocked': return 'Blocked'
    case 'disconnected': return 'Disconnected'
    default: return 'Awaiting runtime status'
  }
}

function gatewayAttachmentLabel(
  gateway: NonNullable<ConnectionStatus['client_ui']>['gateway_attachment'] | undefined,
) {
  if (!gateway) return 'Unavailable'
  // Gateway phases are their own vocabulary. Running them through the
  // reason-code map turned one into a sentence and left the rest title-cased.
  // The endpoint address and port are how this device happened to reach the
  // Tunnel, not something its owner chose or can act on. The state is the fact.
  return GATEWAY_PHASE_TEXT[gateway.state] ?? gateway.state.replace(/_/g, ' ')
}

function meshStateLabel(state: NonNullable<ConnectionStatus['client_ui']>['mesh']['state'] | undefined) {
  switch (state) {
    case 'healthy': return 'Healthy'
    case 'syncing': return 'Syncing'
    case 'degraded': return 'Degraded'
    case 'unavailable': return 'Unavailable'
    default: return 'Unknown'
  }
}

function nativeRoutingStateLabel(state: NonNullable<ConnectionStatus['client_ui']>['native_routing']['state'] | undefined) {
  switch (state) {
    case 'ready': return 'Ready'
    case 'disabled': return 'Disabled'
    case 'applying': return 'Applying'
    case 'needs_helper': return 'Needs helper'
    case 'permission_denied': return 'Repair permissions'
    case 'failed': return 'Failed'
    default: return 'Unknown'
  }
}

function runtimeStateTone(state: string | undefined): 'green' | 'yellow' | 'slate' {
  if (state === 'healthy' || state === 'ready' || state === 'attached') return 'green'
  if (state === 'syncing' || state === 'applying' || state === 'degraded' || state === 'needs_helper' || state === 'permission_denied' || state === 'failed') return 'yellow'
  return 'slate'
}

function remotePeerStateLabel(state: RemotePeerRowV2['state']) {
  switch (state) {
    case 'ready': return 'Ready'
    case 'syncing': return 'Syncing'
    case 'stale': return 'Stale'
    case 'unavailable': return 'Unavailable'
  }
}

function routingStateLabel(state: RemotePeerRowV2['routing']) {
  switch (state) {
    case 'ready': return 'Routing ready'
    case 'syncing': return 'Routing syncing'
    case 'unavailable': return 'Routing unavailable'
    default: return 'Routing unknown'
  }
}

function exportPlacementLabel(placement: RemotePeerRowV2['exports'][number]['placement']) {
  if (!placement) return 'Placement unavailable'
  if (placement.state === 'active_here') return 'Active here'
  return `Standby #${placement.position} here`
}




function formatNativeStatus(status: ConnectionStatus, proxyStatus: ProxyStatus, productInfo: ProductInfo) {
  const ui = status.client_ui
  return JSON.stringify(
    {
      product: productInfo,
      connection: ui
        ? {
            overall: ui.overall,
            overall_reason_code: ui.overall_reason_code,
            gateway_attachment: {
              state: ui.gateway_attachment.state,
              reason_code: ui.gateway_attachment.reason_code,
            },
            mesh: ui.mesh,
            gossip: ui.gossip,
            native_routing: ui.native_routing,
            peer_directory: {
              state: ui.peer_directory.state,
              reason_code: ui.peer_directory.reason_code,
              known_remote_peer_count: ui.peer_directory.peers.length,
            },
            traffic: ui.traffic,
          }
        : { overall: 'unavailable' },
      proxy: {
        running: proxyStatus.running,
        tun_running: proxyStatus.tun_running,
        native_route_count: proxyStatus.tun_routes?.length || 0,
      },
    },
    null,
    2,
  )
}

function buildLogText(nativeStatusText: string, logs: string[]) {
  return [`Native status:\n${nativeStatusText}`, logs.join('\n')].filter(Boolean).join('\n\n')
}

function trafficLabel(tx: number, rx: number) {
  return `${formatBytes(tx)} ↑ / ${formatBytes(rx)} ↓`
}

/** How long the current connection has been up. The engine clears it on
 *  disconnect, so 0 is the honest reading rather than a missing one. */
function formatUptime(totalSeconds: number) {
  const total = Math.max(0, Math.floor(totalSeconds || 0))
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const seconds = total % 60
  if (hours > 0) return `${hours}h ${minutes}m ${seconds}s`
  if (minutes > 0) return `${minutes}m ${seconds}s`
  return `${seconds}s`
}


function formatBytes(bytes: number) {
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let value = Math.max(0, bytes || 0)
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return unit === 0 ? `${Math.floor(value)} ${units[unit]}` : `${value.toFixed(1)} ${units[unit]}`
}

/**
 * An explanation that is there when asked for.
 *
 * Every setting used to carry its sentence underneath, so the tab was a wall
 * of small grey text and the names had grown into sentences to compensate.
 */
function InfoHint(props: { text: string }) {
  const [open, setOpen] = useState(false)
  return (
    <>
      <button
        type="button"
        aria-label="What this does"
        aria-expanded={open}
        onClick={(event) => { event.preventDefault(); setOpen(!open) }}
        className="ml-1.5 inline-flex h-4 w-4 items-center justify-center rounded-full border border-border bg-surface-subdued p-0 text-[10px] leading-none text-content-muted align-middle"
      >
        i
      </button>
      {open && (
        <p className="mt-1 text-xs leading-snug text-content-muted break-words">{props.text}</p>
      )}
    </>
  )
}

function Toggle(props: { label: string; desc: string; checked: boolean; disabled?: boolean; onChange: (v: boolean) => void }) {
  // A <label> binds to the first labelable element inside it, and <button> is
  // labelable. Wrapping the whole row bound every switch to InfoHint's button
  // instead of its checkbox, so no switch could be clicked. The binding is
  // explicit now, and the info button sits outside any label.
  const id = useId()
  const rowClass = `grid grid-cols-[minmax(0,1fr)_auto] items-center gap-4 py-2 ${props.disabled ? 'opacity-50' : ''}`
  const hitClass = props.disabled ? 'cursor-not-allowed' : 'cursor-pointer'
  return (
    <div className={rowClass}>
      <div className="min-w-0">
        <label htmlFor={id} className={`text-sm font-medium ${hitClass}`}>{props.label}</label>
        <InfoHint text={props.desc} />
      </div>
      <label htmlFor={id} className={`relative shrink-0 ${hitClass}`}>
        <input id={id} type="checkbox" checked={props.checked} disabled={props.disabled} onChange={(e) => props.onChange(e.target.checked)} className="sr-only peer" />
        <div className="w-11 h-6 bg-surface-subdued border border-border rounded-full peer peer-checked:bg-accent peer-checked:border-accent peer-focus-visible:ring-2 peer-focus-visible:ring-focus peer-focus-visible:ring-offset-1 transition-colors" />
        <div className="absolute left-1 top-1 w-4 h-4 bg-surface rounded-full peer-checked:translate-x-5 transition-transform" />
      </label>
    </div>
  )
}
