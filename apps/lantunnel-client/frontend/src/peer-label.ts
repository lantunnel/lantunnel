import type { ImportedPeerSummaryV2 } from './client-api'

/**
 * How much of one name survives in a profile row.
 *
 * The selector is a native `<select>` inside a 448px column, so a Tunnel named
 * after a sentence would push the Overlay IP — the only part that is always
 * present and always unambiguous — off the end of the row.
 */
export const MAX_NAME_CHARS = 32

export function truncateName(value: string, max = MAX_NAME_CHARS): string {
  const trimmed = value.trim()
  if (trimmed.length <= max) return trimmed
  // One character of the budget goes to the ellipsis, so the result is never
  // longer than the caller asked for.
  return `${trimmed.slice(0, Math.max(1, max - 1))}…`
}

/**
 * `Tunnel - peer - 198.18.0.3`.
 *
 * A `.peer` file carries neither name, so both are local and either can be
 * missing; the address is the fallback and is never shortened.
 *
 * Nothing here is trimmed to fit the *closed* control. A native `<select>` has
 * one piece of text for both states, and the open state is a full-width picker
 * with room to spare — budgeting for the narrow one would throw information
 * away in the wide one for no reader's benefit. The closed control clips
 * instead, and the address is repeated beneath it so the part that identifies
 * the profile survives either way.
 *
 * The per-name cap that remains is only a stop against a pasted essay.
 */
export function peerProfileLabel(profile: ImportedPeerSummaryV2): string {
  const parts = [profile.tunnel_name, profile.peer_name]
    .map((name) => name?.trim())
    .filter((name): name is string => Boolean(name))
    .map((name) => truncateName(name, MAX_NAME_CHARS))
  parts.push(profile.overlay_ip)
  return parts.join(' - ')
}

/** The same label, for a Tunnel the Platform lists but this device has not joined. */
export function tunnelPickerLabel(name: string | null | undefined, tunnelId: string, plan?: string | null): string {
  // An unnamed Tunnel still has to be distinguishable, and the first segment of
  // a UUID is enough to tell two of them apart in a list this short.
  const label = name?.trim() ? truncateName(name, 28) : `Tunnel ${tunnelId.slice(0, 8)}`
  return plan ? `${label} (${plan})` : label
}
