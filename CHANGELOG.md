# Changelog

User-facing release notes for Lantunnel.

These notes intentionally focus on visible product changes, reliability
improvements, and upgrade impact. Internal implementation details are omitted.

Lantunnel's public history begins at 2.0.0. Earlier 1.x releases were made
before this repository was opened and are not documented here.

## [Unreleased]

### Changed

- German, Spanish, French, Japanese, Simplified Chinese, and Traditional
  Chinese README and usage guides now match the English setup, validation,
  managed onboarding, and Gateway security guidance.
- The repository now links to the project's Buy Me a Coffee page through
  GitHub's Sponsor button.
- Stable version tags now publish GitHub Releases as the authoritative download
  source, with direct package links, signature and checksum guidance,
  installation modes, a managed quick start, and only that version's changes.

## [2.0.9] - 2026-09-04

### Added

- `lantunnel-gateway init --public-ip <PUBLIC_IP>` now prepares an independent Gateway in
  one local command. It writes the runtime config, a persistent self-signed certificate and
  key, and the Scope directory without contacting lantunnel.app.
- The command defaults to QUIC on UDP 8443 with mapping on UDP 8444; `--mapping-port` selects
  another mapping port, and `lantunnel-admin init-tunnel --gateway-mapping-port` records the
  same fact in independent Peer profiles. Repeating the same command preserves the existing
  identity; changed listener facts at the same config path are refused instead of replacing
  a certificate already pinned by Peer profiles. Hostname and public-CA setups remain manual.

### Fixed

- A settings file the Client could not read no longer leaves this machine open
  to the rest of your Tunnel. Settings that could be read but not applied
  already fell back to refusing everything; a file with a stray character, a
  setting name that no longer exists, or a value of the wrong type skipped that
  fallback and started the Client on defaults instead — and a Client nobody has
  configured is reachable by every Peer holding a profile for its Tunnel. Both
  cases now refuse incoming access and say so in Settings, and a file that is
  simply absent still opens, so a fresh install is reachable as before. Your
  saved file is left untouched either way, so it can be repaired by hand.

## [2.0.8] - 2026-08-31

Your Client now shares the network it is sitting on, so the devices around it
are reachable from the rest of your Tunnel without you having to write the
subnet down.

### Added

- **Export Current LAN**, a new switch in Settings that is on by default. With
  it on, the private network this computer is attached to is offered to the
  other Peers in your Tunnel automatically, and it follows the computer: change
  Wi-Fi, move between home and the office, or plug into a different network, and
  the Export changes with it instead of pointing at a network you have left.
  Only Peers holding a profile for the same Tunnel can use it.
- The LAN Export list in Settings now also shows the automatically shared
  network, marked as coming from this machine's current network, so you can see
  everything this computer is offering in one place.

### Changed

- Turning **Export Current LAN** off withdraws only what it added. The prefixes
  you typed into the LAN Export box are unaffected, and adding your own entries
  does not disable the automatic one.
- A fresh install exports the network it is on. If you would rather choose
  exactly what to share, turn the switch off in Settings; upgrading from an
  earlier release keeps everything you had configured and turns the new switch
  on.

## [2.0.7] - 2026-08-31

Changing network used to cost you direct connections for the rest of the
session. It no longer does.

### Fixed

- Direct links to other Peers stopped coming back once the computer moved to a
  different network. Switching Wi-Fi, renewing an address, or waking from sleep
  left every direct path broken until you disconnected and connected again;
  meanwhile everything fell back to the encrypted relay, which is slower and
  counts against relay use. The Client now notices the move and rebuilds its
  direct networking on the new address on its own. On macOS the same problem
  also filled the log with a repeating connection warning.
- LAN routes learned from other Peers were withdrawn for good when the network
  changed, so an exported LAN stayed unreachable even after the network had
  settled. They return with the rebuilt connection.

## [2.0.6] - 2026-08-28

Turning off Tunnel First stopped native routing on the desktop Client
altogether. Native routing is its own switch now, and Tunnel First only answers
the question it is named for.

### Added

- **Native routing** is its own switch in Settings › Network on the desktop
  Client, next to the state it produces. Tunnel First sits under it and is
  unavailable while it is off — your answer is kept for when you turn it back
  on. A Client that was routing natively before this release keeps doing so;
  there is nothing to set again. Phones are unaffected: they always route
  through their VPN service, so the switch is absent there rather than
  different.

### Fixed

- Turning off Tunnel First stopped native routing on the desktop Client, and
  the Connection tab said so: "Native routing: Disabled". Tunnel First only
  answers which of two overlapping networks wins, but it was also the only
  control on the window that could start native routing at all, so the answer
  "the one here" was read as "no routes anywhere".

## [2.0.5] - 2026-08-28

Setting up a Gateway now takes one command. `lantunnel-gateway onboard --pairing
<artifact>` writes the Gateway's configuration for you and prints the command
that starts it; running `lantunnel-gateway` afterwards needs no arguments. The
same command covers both kinds of Gateway — the pairing artifact says which one
it is.

A Gateway also no longer needs a second service running alongside it, and its
mapping port is yours to choose. It used to be fixed, which left no way off a
port the machine already used for something else. Pick the port when you
register the Gateway; several Gateways can share one machine, each with its own
ports and its own directory.

### Added

- A Gateway waiting to be paired can be handed a fresh pairing file from the
  console at any time, or removed. Losing the one you downloaded is no longer a
  dead end.

### Changed

- Disabling a Gateway now only stops new Tunnels from being placed on it.
  Tunnels already using it keep working, and devices that join later still reach
  the rest of their Tunnel. Retiring is what moves a Tunnel elsewhere.
- Retiring a Gateway finishes immediately, even for a machine that has already
  been switched off. It used to wait for a reply that a decommissioned Gateway
  could never send.
- `lantunnel-gateway fleet onboard` is now `lantunnel-gateway onboard`.
- Gateways registered before this release keep working; a Gateway registered
  from this release on needs this version of the Gateway to onboard.

### Fixed

- A device could be sent to a Gateway that none of its peers were on, leaving it
  unable to reach any of them. 
- Direct connections could fail with no sign of why when a Gateway's mapping
  port was not the expected one.
- The Android build on the download page could lag several releases behind.
- Adding a Gateway from the admin console failed with a blank page.
- Confirming an admin operation twice reported a failure for work that had
  succeeded, and a refused operation would not say why.

## [2.0.4] - 2026-08-26

The Android app is now signed with a release key rather than the key every
Android SDK ships with. If you sideloaded a pre-2.0.4 Android build, uninstall
it before installing this one — Android refuses to replace an app whose
signature has changed, and reports only that the app was not installed.
Uninstalling clears the app's data, so the Peer profile has to be imported
again. Later updates install over the top as usual.

### Fixed

- A phone could not be reached by the other devices in its Tunnel, whatever its
  owner wanted, and neither app said so. The mobile runtime never installed an
  access policy, so it ran on the setting that refuses everything.
- The Android app crashed on every launch. It read its stored settings from a
  property initializer, which runs before Android has given the screen a
  Context.
- Changing any desktop setting while the saved settings file could not be read
  erased the owner's access rules and exported networks, silently. The Settings
  tab now says plainly that the saved settings are not in effect, and leaves
  them alone.
- Following the other devices routed nothing they publish. The list was worked
  out at the moment Connect was pressed, when the runtime is stopped and has
  nothing to report. Both apps now remember what the Tunnel last published.
- Importing a Peer profile for a different Tunnel kept the previous Tunnel's
  networks as routes. Since home networks are nearly always 192.168.x.0/24,
  this usually took the phone's own Wi-Fi down while the tunnel was up.
- Android replaced the network list its owner typed with the one worked out
  from the Tunnel, so turning following off showed networks they never chose.
- "Nothing is reachable through this device" was displayed for a rule set that
  only refused IPv4. Every IPv6 destination was still open.
- A missing local proxy authentication flag was read as "on", which threw on
  the thread that starts the VPN, where nothing surfaces it — the tunnel simply
  never came up.
- The Android and iOS status screens said "Unknown" for the connected Peer on a
  healthy connection. The Peer row was running the Peer ID through a formatter
  that does not recognise it.
- The Linux AppImage did not start on some distributions. It shipped its own
  Pango without the matching HarfBuzz, so text shaping resolved against
  whichever version the machine happened to have.
- A fresh Android install died before drawing anything. Two lists that have no
  meaningful default — the published networks and the access rules — were read
  through the parser written for the route list, which hands back the default
  routes when a key is absent. The access rules came back as a network prefix,
  which is not a rule, and reading it threw on the launch path.
- Importing a `.peer` file on Android was refused when the phone already held
  one, and nothing could remove one, so a reinstall or a re-issued profile had
  no way in. Importing replaces, and a profile can be removed.
- Connect crashed the Android app on a rule the app itself had written. An
  older build stored the overlay prefix in the access rules, and building the
  start request from the Connect button threw on it. The prefix is cleared
  when the settings are read, and a line that cannot be read now stops the
  start with the line quoted instead of taking the app down.
- iOS skipped an access rule it could not read on the way to the tunnel, so
  the screen showed a restriction that was never in force. Connect refuses
  with the line quoted, as Android does.

### Added

- A Peers tab on every Client, listing the other devices in the Tunnel with
  their address, how they are reached, what they publish, and a search over all
  of it.
- Android and iOS import a `.peer` file, rather than asking for its contents to
  be pasted in. Android also scans a QR code.
- iOS sends through the tunnel whatever the other devices publish, which
  Android already did. It is on by default on both.

- Every Client shows how long the current connection has been up.
- iOS reads a Peer profile from a QR code. Only Android could.
- Android and iOS can remove an imported Peer profile. Only the desktop could.
- Android and iOS connect when the app opens, once per launch, if a Peer
  profile has been imported. The desktop already did.
- Android and iOS take named access rules — a target, a protocol and a port —
  instead of one switch for everything or nothing. A rule that cannot be read
  stops the start rather than being dropped, so no one is left looking at a
  restriction that is not in force.

### Changed

- The three Clients show one interface. Their screens had been built three
  times over and had drifted apart: the same connection state was worded three
  ways, the same setting sat in a different place under a different name, and
  headings, labels and rows were sized differently on every one. There is one
  set of screens now, and one description of what the connection is doing.
  Where a platform genuinely cannot offer something — a camera on a desktop, a
  login item on a phone, a loopback proxy on a device that already routes every
  app through the tunnel — the control is absent rather than different.
- Android and iOS save a setting as it is changed, as the desktop already did.
  The Apply button is gone.
- "Local network first" is now "Tunnel First". The switch decides which side
  wins when a network is reachable both here and through the Tunnel, and the
  old name said the opposite of what it did. What it does has not changed.
- All three Clients follow the light theme the website uses.
- Access control asks one question instead of three. The Allow list decides who
  can reach this device: leave it empty and every Peer in the Tunnel can, name
  anything and only that can. Blocked rules always win. The separate default
  selector is gone, and rules are edited as rules rather than as JSON.
- The Android and iOS Clients no longer display the imported Peer profile. It
  contains the device's private key. They show the Tunnel, the device and its
  address; replacing a profile means importing another file.
- Android sends through the tunnel whatever the other devices publish, instead
  of asking for a list of networks. The old default, 192.168.0.0/16, also
  covered the Wi-Fi the phone was standing on.
- Nothing in the Client speaks of a plan, a tier or a quota any more. The
  product enforces no ceiling on Peers, networks or relayed traffic, and the
  Client no longer reports or displays one.
- The desktop window fits more on a screen.

- Settings are named rather than described. Every switch on every Client
  carried its sentence underneath, and the names had grown into sentences to
  compensate. The names are back — LAN P2P, LAN Export, Access, Block all,
  Auto-connect, Tunnel First, Start at login — and the explanation is there
  when it is asked for.
- Auto-connect no longer claims to use saved credentials. There are none; it
  reconnects the Peer profile selected last time.

### Removed

- The "Follow mesh" switch. Following is always on: which of two overlapping
  networks wins is already decided by Tunnel First, so the switch gated
  nothing, and the hand-typed network list it revealed could never apply.
- The mobile "refuse incoming connections" switch. It was removed but kept
  writing its last value into the access policy, so a phone that had ever been
  switched on stayed permanently unreachable.
- The Static Gateway override. A Client could set its own dial address, server
  name and trusted certificate while a signed Peer profile was nominally in
  force — and the membership signature covers the Tunnel, the Peer, its address
  and its key, not the Gateway. The `.peer` file is now the only source, and
  importing a different file is how the Gateway changes. A stale override file
  left by an older Client is ignored.

## [2.0.3] - 2026-08-23

### Added

- The Android app opens a `.peer` file. Tap one in a file manager, share one
  from a chat, or pick one with the new button next to the QR scanner. Until now
  the only way in was to paste the file's contents into a text box.

### Changed

- The desktop, Android and iOS Clients now use the same light theme as the
  website, from the same design tokens. All three were dark, and their primary
  colour was a green that appeared nowhere else in the product.
- A Client that has never had access rules configured is now reachable by the
  Peers in its Tunnel, instead of refusing them. Reaching a Client already
  requires an issued Peer profile for the same Tunnel, so denying on top of that
  added no boundary — it only made a fresh install silently unreachable. An
  explicit deny rule, or switching the default back, still closes it. Settings
  that fail to load are unchanged: those still fall back to refusing everything,
  so an unreadable file can never widen access.
- The Android and iOS apps report their real version. Both still reported a
  1.x version while the desktop Client was on 2.0.x.

## [2.0.2] - 2026-08-22

### Added

- Gateways now apply the Fleet Relay allowance the platform assigns them. A
  Tunnel that reaches its monthly allowance stops relaying new data through a
  platform Gateway until the month rolls over or more capacity is bought. Direct
  peer-to-peer traffic and traffic through your own Gateway are unaffected and
  count towards nothing.
- A Gateway records the allowance it was given, so an operator can confirm what
  a Gateway is enforcing instead of waiting for it to run out.

## [2.0.1] - 2026-08-21

### Fixed

- Strengthened Client and Gateway connection handling against malformed or unusually fragmented network traffic.
- Saving Tunnel or Gateway settings on Windows no longer fails with “Access is denied” when another program holds the file for a moment.

### Upgrade notes

- This update is recommended for everyone using version 2.0. Peer profiles and existing settings do not need to be recreated.
- The Windows installer remains unsigned, so Windows may still show “Unknown publisher” or a Microsoft Defender SmartScreen warning.

## [2.0.0] - 2026-08-20

### Changed

- Getting connected is simpler: import a Peer profile and Lantunnel automatically finds an available connection path.
- Managed Gateway support can select and prepare an available Gateway when you connect, reducing setup for everyday use.
- Connections recover more reliably after a temporary network interruption or a Client or Gateway restart.
- Lantunnel Client is available for Windows, macOS, and Linux, with Gateway and Admin tools for people who prefer to self-host.

### Upgrade notes

- Version 2.0 uses new profiles. Version 1 profiles must be recreated before upgrading.
- The Windows installer in this release is unsigned, so Windows may show "Unknown publisher" or a Microsoft Defender SmartScreen warning. Download it only from lantunnel.app or the official GitHub release.
