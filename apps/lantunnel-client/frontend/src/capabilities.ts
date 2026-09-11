/**
 * What this Client can actually do.
 *
 * The rule is that every Client shows the same screens, in the same order,
 * worded the same way. A flag here is not a style choice — it means the
 * platform genuinely cannot offer the thing, or genuinely has no use for it.
 * Anything that merely *looks* different is a bug, not a capability.
 */

import { hostKind, invoke } from './bridge'

export interface Capabilities {
  /** A camera that can read a Peer profile QR. */
  qrScanner: boolean
  /** Start the Client when the machine logs in. */
  startAtLogin: boolean
  /** A SOCKS5 port on loopback for other apps on the same machine. */
  localProxy: boolean
  /** Report whether each exported LAN prefix is published. */
  exportReadiness: boolean
  /** Choose whether this machine installs native routes for the Tunnel. */
  nativeRoutingSwitch: boolean
  /** Sign in to the Platform and add a Peer without leaving the Client. */
  platformAccount: boolean
}

const DESKTOP: Capabilities = {
  qrScanner: false,
  startAtLogin: true,
  localProxy: true,
  exportReadiness: true,
  nativeRoutingSwitch: true,
  platformAccount: true,
}

/**
 * A phone routes every app through its VPN service, so a loopback SOCKS5 port
 * would serve nobody, there is no login item to set, and native routing is not
 * a choice — the VPN service is the only way to reach other apps' traffic at
 * all. The Peer list is not a capability: a phone that holds one profile
 * renders the same list the desktop does, with one row in it.
 */
const PHONE: Capabilities = {
  qrScanner: true,
  startAtLogin: false,
  localProxy: false,
  exportReadiness: false,
  nativeRoutingSwitch: false,
  // A phone has a browser to approve in and a private store to keep the token
  // in — the Keychain on iOS, app-private preferences on Android, beside the
  // Peer key already there. The device grant exists for devices that cannot
  // host a redirect, which is exactly this one.
  platformAccount: true,
}

export function fallbackCapabilities(): Capabilities {
  return hostKind() === 'desktop' ? { ...DESKTOP } : { ...PHONE }
}

/**
 * The host is the authority; the table above is only what to draw before it
 * answers, and after a host too old to be asked.
 */
export async function loadCapabilities(): Promise<Capabilities> {
  try {
    const reported = await invoke<Partial<Capabilities>>('get_capabilities')
    return { ...fallbackCapabilities(), ...reported }
  } catch {
    return fallbackCapabilities()
  }
}
