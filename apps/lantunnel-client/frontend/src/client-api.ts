import { invoke, listen, pickPeerProfile, type Unlisten } from './bridge'

export type { Capabilities } from './capabilities'

export interface HeartbeatStatus {
  active: boolean
  last_time?: number | null
  last_error?: string | null
}

export type ClientOverallStateV2 =
  | 'disconnected'
  | 'starting'
  | 'waiting_for_gateway'
  | 'connected'
  | 'degraded'
  | 'blocked'

export type GatewayAttachmentStateV2 =
  | 'unknown'
  | 'resolving_through_platform'
  | 'provisioning_scope'
  | 'connecting'
  | 'attached'
  | 'unavailable'
  | 'rejected'
  | 'tls_failed'

export type MeshStateV2 = 'unknown' | 'syncing' | 'healthy' | 'degraded' | 'unavailable'
export type GossipStateV2 = 'unknown' | 'syncing' | 'ready' | 'repairing' | 'unavailable'
export type NativeRoutingStateV2 =
  | 'unknown'
  | 'disabled'
  | 'applying'
  | 'ready'
  | 'needs_helper'
  | 'permission_denied'
  | 'failed'
export type NativeRoutingActionV2 = 'install_helper' | 'repair_permissions' | 'retry_apply'
export type PeerDirectoryStateV2 = 'syncing' | 'ready' | 'unavailable'
export type RemotePeerStateV2 = 'syncing' | 'ready' | 'stale' | 'unavailable'
export type PeerCurrentPathV2 = 'direct' | 'encrypted_relay'
export type RoutingStateV2 = 'unknown' | 'syncing' | 'ready' | 'unavailable'

export interface RemotePeerExportV2 {
  prefix: string
  placement?:
    | { state: 'active_here' }
    | { state: 'standby_here'; position: number }
    | null
}

export interface RemotePeerRowV2 {
  peer_id: string
  /** Absent until this Peer's Tunnel-signed membership arrives. */
  overlay_cidr?: string | null
  state: RemotePeerStateV2
  reason_code?: string | null
  current_path?: PeerCurrentPathV2 | null
  routing: RoutingStateV2
  exports: RemotePeerExportV2[]
}

export interface ClientUiStatusV2 {
  overall: ClientOverallStateV2
  overall_reason_code?: string | null
  gateway_attachment: {
    state: GatewayAttachmentStateV2
    endpoint?: string | null
    reason_code?: string | null
  }
  this_peer?: { peer_id: string; overlay_cidr: string } | null
  mesh: { state: MeshStateV2; reason_code?: string | null }
  gossip: { state: GossipStateV2; reason_code?: string | null }
  native_routing: {
    state: NativeRoutingStateV2
    reason_code?: string | null
    actions: NativeRoutingActionV2[]
  }
  peer_directory: {
    state: PeerDirectoryStateV2
    reason_code?: string | null
    peers: RemotePeerRowV2[]
  }
  traffic: {
    direct_tx_bytes: number
    direct_rx_bytes: number
    relay_tx_bytes: number
    relay_rx_bytes: number
  }
  /** What the Platform last reported about this Tunnel's Relay allowance. */
  relay_usage?: {
    used_bytes: number
    allowance_bytes: number
  }
}

export interface ConnectionStatus {
  connected: boolean
  connecting: boolean
  gateway_name?: string | null
  gateway_addr?: string | null
  message: string
  error?: string | null
  platform_heartbeat: HeartbeatStatus
  transport_heartbeat: HeartbeatStatus
  uptime_secs: number
  path_mode: 'disconnected' | 'connecting' | 'relay' | 'p2p'
  p2p_state?: string | null
  p2p_active_sessions?: number
  p2p_primary_peer_id?: string | null
  p2p_peer_count?: number
  traffic?: {
    relay_tx_bytes: number
    relay_rx_bytes: number
    p2p_tx_bytes: number
    p2p_rx_bytes: number
  }
  client_ui?: ClientUiStatusV2
}

export interface ImportedPeerSummaryV2 {
  tunnel_id: string
  peer_id: string
  overlay_ip: string
  bootstrap_kind: 'static_gateway' | 'managed_platform'
  /** Local only. A `.peer` file has never carried a name for either. */
  tunnel_name?: string | null
  peer_name?: string | null
}

export interface PlatformAccountStatusV2 {
  signed_in: boolean
  email?: string | null
  /** Unix seconds. When the stored sign-in stops working. */
  expires_at_unix?: number | null
  platform_url: string
}

export interface DeviceSignInStartV2 {
  user_code: string
  verification_uri: string
  expires_in: number
  interval: number
  /** False on a host with no browser; the owner opens the URL themselves. */
  browser_opened: boolean
}

export type DeviceSignInPollV2 =
  | { state: 'not_started' }
  | { state: 'pending' }
  /** RFC 8628 §3.5: add five seconds to the interval and keep waiting. */
  | { state: 'slow_down' }
  | { state: 'granted'; email?: string | null; expires_at_unix: number }
  | { state: 'denied' }
  | { state: 'expired' }

/** A Peer the Tunnel has already issued, from the Platform's own list. */
export interface PlatformPeerV2 {
  peer_id: string
  name?: string | null
  overlay_ip?: string | null
  created_at?: string | null
}

export interface PlatformTunnelV2 {
  tunnel_id: string
  name?: string | null
  status?: string | null
  billing?: {
    grant_kind?: string | null
    access?: string | null
    effective_plan?: string | null
  } | null
  placement?: { type?: string | null } | null
}

export interface GatewayBootstrapV2 {
  transport: 'quic' | 'websocket' | 'grpc'
  dial_address: string
  port: number
  tls_server_name?: string | null
  trusted_certificate_pem?: string | null
}

export type DesktopNetworkMode = 'socks5_only' | 'lan_routes_tun'

export interface LocalServiceExport {
  route_kind: 'overlay' | 'peer_lan_host'
  protocol: 'tcp' | 'udp'
  ingress_port: number
  source_policy: { type: 'any_tunnel_peer' } | { type: 'only'; peers: string[] }
  local_host: string
  local_port: number
}

export type SettingAvailabilityV2 = 'ready' | 'unavailable'

export interface SettingValueV2<T> {
  availability: SettingAvailabilityV2
  value?: T | null
  reason_code?: string | null
}

export interface ClientAccessPolicyV2 {
  /** Empty means every Peer in the Tunnel may reach this device. */
  allow: ClientAccessRuleV2[]
  /** Always wins over allow. */
  deny: ClientAccessRuleV2[]
}

export interface ClientAccessRuleV2 {
  target: { type: 'this_peer' } | { type: 'ip' | 'cidr' | 'host'; value: string }
  protocol: 'tcp' | 'udp'
  port: { type: 'any' } | { type: 'exact'; value: number }
}

export interface ClientSettingsUiV2 {
  sections: ['connection', 'network_and_lan_export', 'client_access', 'diagnostics']
  tunnel_first: SettingValueV2<boolean>
  exported_lans: SettingValueV2<string[]>
  client_access: SettingValueV2<ClientAccessPolicyV2>
}

export interface LocalExportStatusV2 {
  prefix: string
  ready: boolean
}

export interface AppSettings {
  auto_start: boolean
  auto_connect: boolean
  local_socks5_listen: string
  local_proxy_enabled?: boolean
  /** Whether this machine installs native routes for the Tunnel. */
  desktop_network_mode?: DesktopNetworkMode
  p2p_allow_lan_candidates?: boolean
  local_service_exports?: LocalServiceExport[]
  log_level?: string
  client_access: ClientAccessPolicyV2
  exported_lans: string[]
  /** Export the networks this machine is on without naming them. */
  auto_export_current_lan: boolean
  tunnel_first: boolean
  exported_lan_statuses: LocalExportStatusV2[]
  /** The saved V2 block does not compile, so none of it is in effect. */
  v2_settings_rejected?: boolean
  client_ui?: ClientSettingsUiV2
}

export interface ProxyStatus {
  running: boolean
  listen_addr: string
  tun_running?: boolean
  tun_routes?: string[]
}

export interface ProductInfo {
  binary_name: string
  display_name: string
  role: 'peer'
  version: string
}

export interface TunHelperStatus {
  installed: boolean
  running: boolean
  version?: string | null
  message: string
}

export const api = {
  listPeerProfiles: () => invoke<ImportedPeerSummaryV2[]>('list_peer_profiles'),
  forgetPeerProfile: (tunnelId: string) =>
    invoke<ImportedPeerSummaryV2[]>('forget_peer_profile', { tunnelId }),
  /** Runs the host's own picker — a file dialog, or a phone's document UI. */
  pickPeerProfile: () => pickPeerProfile<ImportedPeerSummaryV2>(),
  /** Reads a Peer profile from a QR code. Hosts with a camera only. */
  scanPeerProfile: () => invoke<ImportedPeerSummaryV2 | null>('scan_peer_profile'),
  connectPeerProfile: (tunnelId: string) =>
    invoke<void>('connect_peer_profile', { tunnelId }),
  disconnect: () => invoke<void>('disconnect'),
  getStatus: () => invoke<ConnectionStatus>('get_status'),
  getProxyStatus: () => invoke<ProxyStatus>('get_proxy_status'),
  getClashConfig: () => invoke<string>('get_clash_config'),
  writeClipboardText: (text: string) => invoke<void>('write_clipboard_text', { text }),
  getSettings: () => invoke<AppSettings>('get_settings'),
  saveSettings: (settings: AppSettings) =>
    invoke<void>('save_settings', { settings }),
  getLogs: (limit?: number) => invoke<string[]>('get_logs', { limit }),
  clearLogs: () => invoke<void>('clear_logs'),
  getProductInfo: () => invoke<ProductInfo>('get_product_info'),
  installTunHelper: () => invoke<TunHelperStatus>('install_tun_helper'),

  setPeerLabels: (tunnelId: string, tunnelName: string | null, peerName: string | null) =>
    invoke<ImportedPeerSummaryV2[]>('set_peer_labels', { tunnelId, tunnelName, peerName }),
  platformAccountStatus: () => invoke<PlatformAccountStatusV2>('platform_account_status'),
  platformStartSignIn: () => invoke<DeviceSignInStartV2>('platform_start_sign_in'),
  platformPollSignIn: () => invoke<DeviceSignInPollV2>('platform_poll_sign_in'),
  platformSignOut: () => invoke<void>('platform_sign_out'),
  platformListTunnels: () => invoke<PlatformTunnelV2[]>('platform_list_tunnels'),
  platformListPeers: (tunnelId: string) =>
    invoke<PlatformPeerV2[]>('platform_list_peers', { tunnelId }),
  platformImportPeer: (
    tunnelId: string,
    peerId: string,
    tunnelName: string | null,
    peerName: string | null,
  ) =>
    invoke<ImportedPeerSummaryV2>('platform_import_peer', {
      tunnelId,
      peerId,
      tunnelName,
      peerName,
    }),
  platformCreatePeer: (tunnelId: string, tunnelName: string | null, peerName: string) =>
    invoke<ImportedPeerSummaryV2>('platform_create_peer', { tunnelId, tunnelName, peerName }),

  onStatus: (fn: (s: ConnectionStatus) => void): Promise<Unlisten> =>
    listen<ConnectionStatus>('status', fn),
  onLog: (fn: (line: string) => void): Promise<Unlisten> => listen<string>('log', fn),
}
