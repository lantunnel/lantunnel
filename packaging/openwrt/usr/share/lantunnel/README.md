# Lantunnel Client on OpenWrt

This package runs the Client headless as an **exporting Peer**: the router
publishes the LAN it already sits on, so the NAS, the printer and the dashboard
behind it become reachable to the rest of the Tunnel — without installing
anything on those devices.

Exporting a subnet only needs outbound dials. There is no TUN device, no
`kmod-tun`, no `ip-full`, and no route or firewall change.

## Pick the right file

```sh
uname -m
```

| `uname -m` | file |
|---|---|
| `aarch64` | `lantunnel-client-openwrt-<version>-aarch64.tar.gz` |
| `armv7l` | `lantunnel-client-openwrt-<version>-armv7.tar.gz` |
| `x86_64` | `lantunnel-client-openwrt-<version>-x86_64.tar.gz` |

The binary is statically linked against musl, so it needs no libraries from the
package feeds. Installed it is 5.5MB on armv7, 5.7MB on aarch64 and 7.1MB on x86_64 — more
than a 16MB-flash router (most `ath79` and `ramips` devices) has room for. MIPS is not built at all.

## Install

```sh
tar -xzf lantunnel-client-openwrt-<version>-<arch>.tar.gz -C /
```

That writes `/usr/bin/lantunnel-client`, `/etc/init.d/lantunnel` and
`/etc/config/lantunnel`.

## Import the Peer profile

Create the `.peer` file on the trusted machine that holds the `.tunnel` file
(`lantunnel-admin add-peer`), copy it to the router, then:

```sh
TUNNEL_PROXY_APP_CONFIG_DIR=/etc/lantunnel \
  /usr/bin/lantunnel-client tunnel import /tmp/router.peer

TUNNEL_PROXY_APP_CONFIG_DIR=/etc/lantunnel \
  /usr/bin/lantunnel-client tunnel list
```

Delete the copy under `/tmp` afterwards — the profile carries the Peer's private
key, and the import already stored its own owner-only copy.

With exactly one profile imported, the init script connects it. With more than
one, name it:

```sh
uci set lantunnel.main.tunnel_id=<TUNNEL_ID>
uci commit lantunnel
```

## Start it

```sh
/etc/init.d/lantunnel enable
/etc/init.d/lantunnel start
```

Check it:

```sh
TUNNEL_PROXY_APP_CONFIG_DIR=/etc/lantunnel \
  /usr/bin/lantunnel-client status --json

logread -e lantunnel
```

## What gets exported

`auto_export_current_lan` is on by default, so whatever private subnets the
router is attached to are published — usually `192.168.1.0/24` and nothing else
to configure. To publish a fixed list instead, set `exported_lans` in
`/etc/lantunnel/app/settings.json` and restart the service.

Who may reach those addresses is the Client's access policy, also in
`settings.json`. An empty allow-list means every member of the Tunnel.

## Logs

procd sends stdout and stderr to syslog (`logread -e lantunnel`). The rotating
log files go to `/var/log/lantunnel`, which is tmpfs — deliberately, so a daily
log file does not wear out the flash. They do not survive a reboot.

## Surviving sysupgrade

`/etc/config/lantunnel` is kept automatically. The imported profile is not, so
add it to the keep list:

```sh
echo /etc/lantunnel >> /etc/sysupgrade.conf
```

The binary is never kept — reinstall the tarball after an upgrade.

## Uninstall

```sh
/etc/init.d/lantunnel stop
/etc/init.d/lantunnel disable
rm -rf /usr/bin/lantunnel-client /etc/init.d/lantunnel /etc/config/lantunnel \
       /etc/lantunnel /var/log/lantunnel
```
