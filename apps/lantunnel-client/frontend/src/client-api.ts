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
  overlay_cidr: string
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

  onStatus: (fn: (s: ConnectionStatus) => void): Promise<Unlisten> =>
    listen<ConnectionStatus>('status', fn),
  onLog: (fn: (line: string) => void): Promise<Unlisten> => listen<string>('log', fn),
}
