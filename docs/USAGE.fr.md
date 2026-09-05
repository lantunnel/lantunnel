# Utiliser Lantunnel

Un guide pratique : se connecter, atteindre ses machines, et garder la main sur qui atteint les vôtres.

Vous découvrez le projet ? Commencez par le [README](../README.fr.md). Vous cherchez la conception sous-jacente ? [CONTEXT.md](../CONTEXT.md).

[English](./USAGE.md) ·
[简体中文](./USAGE.zh-CN.md) ·
[繁體中文](./USAGE.zh-TW.md) ·
[日本語](./USAGE.ja.md) ·
[Español](./USAGE.es.md) ·
[Deutsch](./USAGE.de.md) ·
**Français**

**Sommaire**

1. [L'idée en une minute](#lidée-en-une-minute)
2. [Choisir sa voie](#choisir-sa-voie)
3. [Voie A — Gateway hébergée (le plus rapide)](#voie-a--gateway-hébergée-le-plus-rapide)
4. [Voie B — Gateway autohébergée](#voie-b--gateway-autohébergée)
5. [Atteindre vos services](#atteindre-vos-services)
6. [Partager tout un LAN](#partager-tout-un-lan)
7. [Décider qui vous atteint](#décider-qui-vous-atteint)
8. [Sur un serveur (headless)](#sur-un-serveur-headless)
9. [Téléphones](#téléphones)
10. [Référence des commandes](#référence-des-commandes)
11. [Référence des réglages](#référence-des-réglages)
12. [Où se trouvent les fichiers](#où-se-trouvent-les-fichiers)
13. [Dépannage](#dépannage)

---

## L'idée en une minute

Un **Tunnel** est un petit réseau privé de machines qui se font mutuellement confiance. Chaque machine qui en fait partie est un **Peer**, et chaque Peer détient un **profil `.peer`** signé : son identité, sa clé privée, et la manière de trouver la Gateway.

Dès que deux Peers appartiennent au même Tunnel, ils dialoguent directement chaque fois que le réseau le permet. Sinon, ils se replient sur un relais via la **Gateway**, qui transmet des octets scellés qu'elle ne peut pas lire. La Gateway est un point de rendez-vous, pas un intermédiaire.

Trois choses dont vous n'aurez jamais besoin : une IP publique sur votre LAN, un port redirigé sur la box, ou un mot de passe partagé.

## Choisir sa voie

|  | **Gateway hébergée** | **Gateway autohébergée** |
|---|---|---|
| Ce que vous faites tourner | le Client seul | le Client et votre propre Gateway |
| Ce qu'il vous faut | un compte sur [lantunnel.app](https://lantunnel.app/) | une machine avec une adresse publique |
| Temps de mise en route | quelques minutes | une vingtaine de minutes |
| Relais | 5 Go par mois offerts, décompté au-delà | le vôtre, sans décompte |
| P2P direct | illimité | illimité |

Les deux voies utilisent le même Client et le même protocole. Vous pouvez commencer en hébergé puis migrer, ou faire tourner les deux en parallèle : un Tunnel est indépendant de tout compte.

---

## Voie A — Gateway hébergée (le plus rapide)

**[lantunnel.app](https://lantunnel.app/)** exploite la flotte de Gateways à votre place. Chaque compte dispose d'un Tunnel gratuit permanent : trafic pair à pair illimité, nombre illimité d'appareils LAN derrière chaque Client, et 5 Go de relais chiffré par mois pour les cas où la liaison directe échoue.

1. **Créez un Tunnel** — inscrivez-vous sur [lantunnel.app](https://lantunnel.app/) et créez votre Tunnel gratuit. Aucune adresse de Gateway, aucun certificat, aucun DNS à configurer.
2. **Ajoutez un Peer par appareil** — un pour le portable, un pour le NAS, un pour le poste fixe. Téléchargez chaque profil `.peer`.
3. **Installez le Client** — depuis [lantunnel.app/download](https://lantunnel.app/download), ou en le compilant depuis ce dépôt.
4. **Importez et connectez :**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # ouvre l'interface ; connectez-vous depuis là
   ```

Un profil géré ne contient que l'URL de la plateforme. À la connexion, le Client demande sur quelle Gateway se trouve actuellement son Tunnel, signe la requête avec sa propre clé et récupère les paramètres de connexion. Si la Gateway change, rien n'est à modifier sur vos appareils.

Vous pouvez passer directement à [Atteindre vos services](#atteindre-vos-services).

---

## Voie B — Gateway autohébergée

Tout ce qui suit se trouve dans ce dépôt sous licence Apache-2.0. Rien ne contacte lantunnel.app.

Séparez bien ces deux rôles de machine :

- **Hôte de la Gateway :** la machine publique contient le binaire de la Gateway, la paire de clés TLS et les fichiers publics `.scope`.
- **Machine de confiance du propriétaire :** elle contient `lantunnel-admin`, le fichier privé `.tunnel` du propriétaire et les fichiers `.peer` de chaque installation avant leur transfert vers chaque Client.

N'installez jamais `lantunnel-admin` et ne stockez aucun fichier `.tunnel` ou `.peer` sur l'hôte public de la Gateway.

### Ce qu'il vous faut

- Une machine joignable depuis internet — un VPS à 5 dollars suffit largement : la Gateway fait surtout de la signalisation, et le relais ne transporte que ce que la liaison directe n'arrive pas à acheminer.
- Deux règles entrantes dessus : votre **port de données** (TCP ou UDP selon le transport) et le **port UDP de mappage** choisi (`8444` par défaut).
- Une adresse IPv4 ou IPv6 publique fixe. La voie principale génère l'identité TLS ; les noms d'hôte et certificats reconnus publiquement utilisent la voie manuelle avancée.

### 1. Compilez (ou téléchargez) les binaires

```bash
# Hôte de la Gateway
cargo build --release -p lantunnel-gateway

# Machine de confiance du propriétaire
cargo build --release -p lantunnel-admin

# Exécutez ceci dans chaque shell de compilation après la commande correspondante.
export PATH="$PWD/target/release:$PATH"
```

Installez [Lantunnel Client](https://lantunnel.app/download) sur chaque appareil Peer. Pour compiler le Client, suivez les commandes frontend et Rust de la section [Compiler depuis les sources](../README.fr.md#compiler-depuis-les-sources).

### 2. Initialisez la Gateway indépendante

Sur l'hôte de la Gateway, exécutez l'initialisation hors ligne avec son adresse IP publique fixe :

```bash
lantunnel-gateway init --public-ip <PUBLIC_IP>
```

La commande ne contacte ni lantunnel.app ni aucune autre plateforme. Par défaut, elle configure l'écouteur de données QUIC sur UDP `8443`, l'écouteur de mappage sur UDP `8444` et `configs/gateway.yaml`. Utilisez `--transport`, `--data-port`, `--mapping-port` ou `--config` pour changer le transport, le port de données, le port de mappage ou le chemin de configuration.

Elle génère `configs/gateway.yaml`, `certs/server.crt`, `certs/server.key` et `state/scopes.d`. Sous Linux et macOS, les répertoires sont réservés au propriétaire (permissions `0700`) ; la configuration, le certificat et la clé utilisent les permissions `0600`.

Avec le même fichier `--config`, la répétition exacte d'`init`, la validation et le démarrage fonctionnent depuis n'importe quel répertoire de travail.

`certs/server.crt` est le certificat public auto-signé dont le SAN contient exactement cette IP. `certs/server.key` reste exclusivement sur l'hôte de la Gateway. La réexécution strictement identique de la commande conserve la clé, le certificat et la configuration octet pour octet.

Si, pour le même chemin de configuration, l'IP, le transport, le port de données ou le port de mappage diffère de l'état existant, `init` refuse de remplacer quoi que ce soit. Un autre fichier de configuration situé dans la même racine de déploiement peut réutiliser le même certificat compatible.

**Voie manuelle avancée — nom d'hôte ou certificat d'une AC publique.** `init --public-ip` ne gère pas ce cas. Placez la chaîne de certificats et la clé sous `certs/` dans deux fichiers ordinaires distincts (pas de liens symboliques), attribuez les permissions `0600` aux deux et créez `configs/gateway.yaml` en suivant l'étape 5. Pour un certificat de nom d'hôte auto-signé, vous pouvez toujours utiliser :

```bash
mkdir -p certs
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout certs/server.key -out certs/server.crt \
  -subj "/CN=gw.example.com" \
  -addext "subjectAltName = DNS:gw.example.com" \
  -addext "basicConstraints = critical, CA:FALSE" \
  -addext "keyUsage = critical, digitalSignature, keyEncipherment" \
  -addext "extendedKeyUsage = serverAuth"
chmod 0600 certs/server.crt certs/server.key
```

### 3. Créez le Tunnel hors ligne

Depuis l'hôte de la Gateway, copiez uniquement le fichier public `certs/server.crt` sur la machine de confiance du propriétaire ; ne copiez jamais `certs/server.key`. Enregistrez-y la copie publique sous `./server.crt`. `lantunnel-admin` ne communique jamais avec le réseau, et le fichier `.tunnel` qu'il produit est la clé privée de signature du Tunnel.

```bash
mkdir -p provision
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-ip <PUBLIC_IP> \
  --gateway-port 8443 \
  --gateway-mapping-port 8444 \
  --gateway-cert ./server.crt \
  --output-dir ./provision
```

Deux fichiers sont écrits, nommés d'après l'identifiant de Tunnel généré :

| Fichier | Destinataire | Contenu |
|---|---|---|
| `<tunnel-id>.tunnel` | **Vous seul.** Permissions `0600`. | La clé privée de signature du Tunnel. La perdre vous empêche d'émettre d'autres Peers ; la divulguer permet à quelqu'un d'autre de le faire. |
| `<tunnel-id>.scope` | La Gateway. Public. | L'identifiant du Tunnel et la clé *publique* de signature, rien de plus. Il ne permet ni d'émettre des Peers ni de lire le trafic. |

Options de `init-tunnel` :

- `--gateway-transport quic | websocket | grpc` — QUIC est le choix par défaut, et le seul avec un flux dédié par connexion. WebSocket et gRPC servent aux réseaux qui bloquent UDP.
- `--gateway-host` et/ou `--gateway-ip` — si vous fournissez les deux, la connexion est établie via l'adresse IP et le nom d'hôte sert de nom de serveur TLS.
- `--gateway-mapping-port` — le port UDP de mappage de la Gateway. Il vaut `8444` par défaut et doit correspondre à `lantunnel-gateway init --mapping-port` ou à `gateway.mapping_probe_port`.
- `--gateway-cert` — le PEM à épingler. À omettre si la Gateway utilise un certificat reconnu publiquement.

L'exemple épingle le certificat généré par `init`. Dans la voie manuelle avancée avec un certificat de nom d'hôte reconnu publiquement, utilisez `--gateway-host` et omettez `--gateway-cert` afin que les renouvellements ordinaires du certificat n'exigent pas de nouveaux profils de Peer.

### 4. Émettez un profil par appareil

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

Chaque `add-peer` attribue une **Overlay IP** dans `198.18.0.0/16`, génère une nouvelle paire de clés, signe l'appartenance et met à jour le fichier propriétaire de façon atomique.

> **Un `.peer` par appareil.** Copier un profil sur une deuxième machine ne duplique pas un Peer : les deux instances se disputent la même identité et la Gateway rejette la perdante.

Options utiles : `--overlay-ip` pour fixer une adresse, `--replicas` pour autoriser ce Peer à ouvrir plusieurs connexions de transport simultanées.

### 5. Lancez la Gateway

Copiez **uniquement le scope public** dans le répertoire généré par `init` sur l'hôte de la Gateway :

```bash
cp /chemin/vers/<tunnel-id>.scope state/scopes.d/
```

Validez la configuration générée, puis démarrez la Gateway :

```bash
lantunnel-gateway --config configs/gateway.yaml --check-config
lantunnel-gateway --config configs/gateway.yaml
```

La configuration générée stocke les chemins des fichiers d'exécution sous forme absolue. Ici, `<DEPLOYMENT_ROOT>` est le répertoire persistant de déploiement contenant le fichier de configuration (ou son répertoire `configs/`) et les répertoires générés `certs/` et `state/`. Dans la voie manuelle avancée, les données de connexion doivent correspondre à celles transmises à `init-tunnel` :

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # doit correspondre à --gateway-port
  transport_type: "quic"          # doit correspondre à --gateway-transport
  tls_cert: "<DEPLOYMENT_ROOT>/certs/server.crt"
  tls_key: "<DEPLOYMENT_ROOT>/certs/server.key"
  scopes_dir: "<DEPLOYMENT_ROOT>/state/scopes.d"    # déposez les fichiers .scope ici
  mapping_probe_port: 8444        # UDP ; valeur par défaut configurable
```

La Gateway ouvre elle-même le port UDP de mappage choisi : il n'y a pas de second processus à lancer. Un écouteur de données QUIC ne peut pas utiliser le même port que l'écouteur UDP de mappage ; WebSocket et gRPC peuvent réutiliser le numéro, car leurs écouteurs de données utilisent TCP.

Vous pouvez modifier `gateway.mapping_probe_port` après `init` et avant d'émettre les profils Peer. Transmettez la même valeur à `lantunnel-admin init-tunnel --gateway-mapping-port` et ouvrez ce port UDP dans le pare-feu.

Pour changer le port ensuite, ouvrez le nouveau port UDP, modifiez `gateway.mapping_probe_port` dans le YAML, puis redémarrez la Gateway.

Modifier uniquement le YAML interrompt les sondes de mappage des profils Peer existants.

Mettez aussi à jour `static_gateway.mapping_port` dans le `.tunnel` existant et `bootstrap.mapping_port` dans chaque `.peer` existant. Réimportez ces profils, puis reconnectez les Clients.

L'identifiant du Tunnel, le `.scope` installé et les signatures d'appartenance Peer restent valides : inutile de recréer un Tunnel ou un Scope, ou de signer à nouveau.

Si le `.peer` d'origine manque, utilisez `add-peer` avec le même `.tunnel` pour créer une nouvelle identité Peer.

Pour ajouter un Tunnel plus tard, il suffit de déposer un autre `.scope` dans `scopes_dir`. Des unités systemd d'exemple se trouvent dans [`scripts/remote/`](../scripts/remote/).

### 6. Connectez les appareils

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # vérification ; n'affiche jamais de clé privée
lantunnel-client                      # interface graphique
lantunnel-client connect <tunnel-id>  # ou en headless
```

---

## Atteindre vos services

Une fois connecté, deux moyens s'offrent à vous pour envoyer du trafic dans le Tunnel.

### 1. Le proxy SOCKS5 local — toujours actif

Tout Client connecté expose un proxy SOCKS5 sur **`127.0.0.1:1080`**, en loopback uniquement et sans authentification. Il n'en a pas besoin : il est lié au loopback, et chaque requête qui le traverse est autorisée par la politique du Peer *de destination*.

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # Jellyfin sur un Peer
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # NAS sur le LAN d'un Peer
```

Les navigateurs, `ssh -o ProxyCommand`, Docker et la plupart des outils en ligne de commande acceptent directement un proxy SOCKS5. Si le port `1080` est pris, déplacez l'écouteur avec `--local-socks5-listen 127.0.0.1:1081`.

Lorsque le Client est connecté, le panneau de réglages du bureau copie un extrait YAML Clash prêt à coller pour cet écouteur.

### 2. Routage natif — toutes les applications, sans configuration

Activez le routage natif et la machine installe de vraies routes pour le Tunnel : *n'importe quelle* application atteint alors les Peers par leur adresse, sans savoir que Lantunnel existe.

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

Vous pouvez aussi changer le mode réseau et ajouter les routes depuis l'interface. Sur un téléphone, la question ne se pose pas : le service VPN est le seul moyen d'atteindre le trafic des autres applications, donc il s'applique toujours.

**Tunnel First** détermine ce qui se passe quand une route distante du Tunnel recouvre le réseau où vous vous trouvez physiquement. Désactivé (par défaut), votre LAN local l'emporte ; activé, c'est le Tunnel — pratique quand le Wi-Fi du café utilise lui aussi `192.168.1.0/24`. Dans les deux cas, la Gateway, le canal de contrôle, le DNS et vos propres destinations exportées restent protégés sur leurs routes natives.

### Quelle adresse utiliser ?

| Pour atteindre | Utilisez |
|---|---|
| Un service sur la machine distante elle-même | Son **Overlay IP** (`198.18.x.y`) et le port du service. `lantunnel-client tunnel list` l'affiche en JSON, et l'interface la montre aussi. |
| Un appareil sur le LAN du Peer distant | L'**adresse LAN réelle** de cet appareil, par exemple `192.168.1.50`. |

Par défaut, un port de l'Overlay est mappé sur `127.0.0.1` au même port sur la machine cible.

---

## Partager tout un LAN

Un Peer peut annoncer les sous-réseaux privés auxquels il est rattaché. Les autres Peers atteignent alors *n'importe quoi* sur ces sous-réseaux à travers lui — le NAS, l'imprimante, l'interface web du switch — sans rien installer sur ces appareils.

Deux sources indépendantes, toutes deux actives par défaut dans l'interface :

- **Exporter le LAN courant** (`auto_export_current_lan`, actif par défaut) publie les réseaux privés auxquels cette machine est rattachée à l'instant, et les recalcule à chaque analyse des interfaces. Emportez le portable de la maison au bureau : l'export suit.
- **Exports saisis à la main** (`exported_lans`) : les préfixes que vous désignez vous-même.

Désactiver l'interrupteur automatique ne retire que ce qu'il avait ajouté ; votre liste manuelle reste intacte.

Seuls les préfixes IPv4 RFC1918 sont acceptés. Les routes par défaut, les plages publiques, le loopback, le lien-local, le multicast et tout ce qui recouvre le pool Overlay sont refusés.

**Exporter crée de l'accessibilité, pas une autorisation.** Un Peer distant doit toujours passer la [politique d'accès](#décider-qui-vous-atteint) du Client exportateur pour chaque destination.

Si deux Peers exportent le même préfixe, chaque Client retient le premier qu'il a vu et bascule vers le suivant lorsque le dernier chemin de celui-ci tombe. C'est une décision propre à chaque Client, non persistée : il est donc normal que deux de vos machines choisissent des exportateurs différents.

---

## Décider qui vous atteint

La **politique d'accès du Client** est la seule ACL de Lantunnel, et elle réside sur la machine que l'on atteint. Ni sur la Gateway, ni sur un serveur. Le choix de route détermine *où* envoyer ; votre Client décide indépendamment s'il répond.

Comportement par défaut : une politique vide signifie que **tout Peer détenant un profil de votre Tunnel peut vous atteindre**. Obtenir ce profil supposait déjà que vous l'ayez émis ; une seconde barrière par-dessus n'ajoutait aucune frontière, elle rendait seulement les installations neuves inexplicablement injoignables. Dès que vous écrivez une règle Allow, elle devient l'unique porte d'entrée. **Deny est toujours évalué en premier et l'emporte toujours.**

Configurez-la dans l'interface, ou dans `settings.json` :

```jsonc
{
  "client_access": {
    "allow": [
      // SSH vers cette machine
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // Jellyfin sur le NAS d'à côté
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // Tout le sous-réseau IoT, n'importe quel port TCP
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // ...et jamais le routeur, quoi que dise la liste Allow
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

Les cibles sont `this_peer`, `ip`, `cidr` ou `host`. Les ports sont `any` ou `exact` — les plages ne sont pas gérées. L'ordre des règles n'a aucune importance ; seul compte le fait que Deny prime sur Allow. Une règle ne désigne jamais un Peer source : tout membre authentifié du Tunnel obtient la même réponse.

Pour tout refuser, interdisez `0.0.0.0/0` et `::/0` en TCP comme en UDP — c'est exactement ce qu'écrit le bouton « bloquer tout le trafic entrant » de l'interface, si bien que le fichier enregistré correspond à ce que vous avez demandé.

---

## Sur un serveur (headless)

`--headless` (alias `--no-ui`) exécute exactement le même runtime, sans fenêtre, sans icône de barre d'état ni WebView : même logique de reconnexion, même comportement de PeerLink et du relais, mêmes interfaces SOCKS5 et TUN.

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # au premier plan, sans interface
lantunnel-client status --json                # depuis un autre terminal
lantunnel-client disconnect
```

`--headless` employé seul se connecte au profil marqué en connexion automatique : l'unité de service n'a donc besoin d'aucun identifiant de Tunnel.

```ini
[Unit]
Description=Lantunnel Client
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/lantunnel-client --headless
Restart=always
RestartSec=5
User=lantunnel
Environment=TUNNEL_PROXY_APP_CONFIG_DIR=/var/lib/lantunnel

[Install]
WantedBy=multi-user.target
```

Le mode headless n'a pas d'interface de réglages : modifiez directement `settings.json` dans le répertoire de configuration — voir la [référence des réglages](#référence-des-réglages).

**Sous Windows**, les builds de release utilisent le sous-système GUI : un lancement normal n'ouvre aucune console et `cmd.exe` n'attend pas le processus. Quand la sortie et le code de retour d'une commande courte comptent, utilisez `start /wait` :

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## Téléphones

Android (`apps/android-proxy`, VpnService) et iOS (`apps/ios-proxy`, NetworkExtension) exécutent le même cœur Rust via `tp-mobile-ffi`. Importez le profil `.peer` en scannant son QR code ou en ouvrant le fichier, puis démarrez le VPN.

Il n'y a pas de sélecteur de mode réseau sur un téléphone : le service VPN est le seul moyen d'atteindre le trafic des autres applications, donc le routage natif suit toujours le runtime.

---

## Référence des commandes

### `lantunnel-client`

```
lantunnel-client                          Ouvre l'interface graphique
lantunnel-client connect <TUNNEL_ID>      Connecte un profil importé, sans interface
lantunnel-client disconnect               Déconnecte le Client en cours d'exécution
lantunnel-client status --json            Affiche l'état en JSON
lantunnel-client tunnel import <FILE>     Importe un profil .peer
lantunnel-client tunnel list              Liste les profils en JSON
```

`tunnel list` affiche, pour chaque profil importé, l'identifiant de Tunnel, l'identifiant de Peer, l'Overlay IP et le type d'amorçage. Le matériel de clé privée n'est pas sérialisable et n'apparaît jamais.

| Option | Signification |
|---|---|
| `--headless`, `--no-ui` | Exécute le runtime complet sans interface |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace` |
| `--local-socks5-listen <ADDR>` | Déplace l'écouteur SOCKS5 de loopback |
| `--desktop-network-mode <MODE>` | `socks5_only` ou `lan_routes_tun` |
| `--lan-route <CIDR>` | Installe une route LAN native (répétable) |
| `--enable-lan-p2p` | Autorise les adresses LAN comme candidates au chemin direct |
| `-V`, `--help` | Version, aide |

Surcharges par variables d'environnement : `LANTUNNEL_LOCAL_SOCKS5_LISTEN`, `LANTUNNEL_DESKTOP_NETWORK_MODE`, `LANTUNNEL_LAN_ROUTES`, `TUNNEL_PROXY_APP_CONFIG_DIR`.

### `lantunnel-admin`

```
lantunnel-admin init-tunnel --gateway-transport <quic|websocket|grpc>
                            [--gateway-host <HOST>] [--gateway-ip <IP>]
                            --gateway-port <PORT>
                            [--gateway-mapping-port <PORT>]
                            [--gateway-cert <PEM>]
                            [--output-dir <DIR>]

lantunnel-admin add-peer --tunnel <FILE.tunnel>
                         [--overlay-ip <IPV4>] [--replicas <N>]
                         [--name <NAME>] [--output <FILE.peer>]
```

Hors ligne par conception. Il refuse les liens symboliques et n'écrase jamais un fichier existant.

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>] [--check-config]       Exécute ou valide la Gateway
lantunnel-gateway init --public-ip <PUBLIC_IP>             Initialise hors ligne une Gateway indépendante par IP
                       [--transport <quic|websocket|grpc>]
                       [--data-port <PORT>] [--mapping-port <PORT>]
                       [--config <FILE>]
lantunnel-gateway onboard --pairing <FILE>       Enrôle une Gateway gérée par la plateforme
lantunnel-gateway mapping serve                  Réflecteur UDP de mappage autonome
```

`init` fonctionne hors ligne et utilise par défaut QUIC sur UDP `8443` ainsi que le mappage sur UDP `8444` ; utilisez `--mapping-port` pour choisir un autre port de mappage. `--config` vaut `configs/gateway.yaml` par défaut. `mapping serve` existe pour des déploiements atypiques ; une Gateway normale ouvre sa propre socket de mappage et n'en a pas besoin.

L'enrôlement géré doit commencer dans un nouveau répertoire de travail accessible uniquement à son propriétaire. La Gateway peut ainsi y écrire sa configuration d'exécution. Suivez le [guide d'installation d'une Gateway connectée à la plateforme](https://lantunnel.app/docs/installation#platform-connected) ou utilisez la même séquence sûre :

```bash
mkdir -m 700 lantunnel-gateway-state
mv /path/to/downloaded-pairing.yaml lantunnel-gateway-state/pairing.yaml
chmod 600 lantunnel-gateway-state/pairing.yaml
cd lantunnel-gateway-state
lantunnel-gateway onboard --pairing pairing.yaml
```

---

## Référence des réglages

`settings.json`, dans le répertoire de configuration du Client. Toutes les clés sont facultatives.

| Clé | Défaut | Signification |
|---|---|---|
| `auto_start` | `false` | Démarrer à l'ouverture de session |
| `auto_connect` | `false` | Se connecter au démarrage |
| `local_proxy_enabled` | `true` | Faire tourner l'écouteur SOCKS5 local |
| `local_socks5_listen` | `"127.0.0.1:1080"` | Son adresse (loopback uniquement) |
| `desktop_network_mode` | `"socks5_only"` | Ou `"lan_routes_tun"` pour les routes natives |
| `lan_routes` | `[]` | Routes natives à installer en mode `lan_routes_tun` |
| `tunnel_first` | `false` | Laisse les routes du Tunnel primer sur les routes LAN locales qui les recouvrent |
| `exported_lans` | `[]` | Préfixes privés que ce Peer publie |
| `auto_export_current_lan` | `true` | Publier aussi les réseaux auxquels cette machine est rattachée |
| `client_access` | ouvert | L'ACL — voir [plus haut](#décider-qui-vous-atteint) |
| `p2p_allow_lan_candidates` | `false` | Proposer les adresses LAN comme candidates au chemin direct |
| `log_level` | `"info"` | Niveau de journalisation du Client |

Les clés inconnues sont refusées plutôt qu'ignorées : une faute de frappe se voit au lieu de rester silencieusement sans effet.

---

## Où se trouvent les fichiers

| Quoi | Chemin |
|---|---|
| Configuration du Client, profils importés, secrets | `~/.lantunnel/app/` (modifiable via `TUNNEL_PROXY_APP_CONFIG_DIR`) |
| Réglages du Client | `~/.lantunnel/app/settings.json` |
| Configuration de la Gateway | `configs/gateway.yaml` (ou via `--config`) |
| Admission des Tunnels par la Gateway | `state/scopes.d/*.scope` |
| Journal d'usage du relais | `state/relay-usage.wal` |
| Fichier propriétaire du Tunnel | là où `init-tunnel --output-dir` l'a écrit — sauvegardez-le |

La clé privée importée est stockée dans un fichier que le Client crée en accès propriétaire uniquement ; elle n'est jamais écrite dans un journal, jamais envoyée à une Gateway, et ne quitte jamais la machine.

---

## Dépannage

**Le Client ne se connecte pas.**
Vérifiez que la Gateway tourne et que son port de données est joignable depuis l'extérieur (`nc -z gw.example.com 8443`, ou `nc -zu` pour QUIC). Confirmez ensuite que le `.scope` du Tunnel se trouve bien dans le `scopes_dir` de la Gateway : sans lui, elle n'a aucune raison de vous admettre.

**« Peer already attached », ou un Client sans cesse éjecté.**
Deux Clients utilisent le même `.peer`. Émettez un second profil avec `add-peer` : un profil est l'identité d'un appareil, pas un identifiant partagé.

**Tout fonctionne, mais toujours en relais.**
Regardez les compteurs de trafic dans l'interface : ils séparent le direct du relais. Un NAT symétrique aux deux extrémités peut faire échouer la perforation. Si les deux Peers sont sur le même LAN, ajoutez `--enable-lan-p2p` pour proposer les adresses locales comme candidates. Vérifiez aussi que le port UDP de mappage configuré (`8444` par défaut) atteint la Gateway : sans la sonde de mappage, aucun des deux Peers ne découvre son mappage public.

**Le direct marche, pas le relais (ou l'inverse).**
Ce sont deux chemins indépendants. Le relais a besoin du port de données de la Gateway ; le direct a besoin que l'UDP circule entre les Peers. Testez-les un à la fois.

**Un service distant refuse la connexion.**
C'est la politique d'accès du Client cible qui refuse : vérifiez `client_access` *sur cette machine-là*, pas sur la vôtre. Un résultat `NotAuthorized` est définitif et ne bascule jamais vers un autre Peer.

**Un LAN exporté est injoignable.**
Le Client exportateur doit être effectivement rattaché à ce réseau pour que l'export soit prêt : un préfixe configuré n'est publié que s'il correspond exactement à un réseau connecté. Vérifiez ensuite que la politique d'accès de ce Client autorise la destination et le port précis.

**Versions incompatibles.**
Les Peers, les Gateways et les profils doivent appartenir à la même lignée 2.0.x. Le format réseau ne se négocie pas entre versions, et un déploiement mixte échoue en se fermant.

**Obtenir plus de détails.**
`--log-level debug` côté Client, `log.level: debug` dans la configuration de la Gateway. Les journaux ne contiennent jamais de clés privées, de contenu de profil ni de clés de session.

---

## Pour aller plus loin

- **[lantunnel.app](https://lantunnel.app/)** — Tunnel gratuit, Gateways gérées, téléchargements, et guides pour le streaming de jeux, les outils d'IA privés et les services domestiques.
- **[CONTEXT.md](../CONTEXT.md)** — comment les pièces s'assemblent et ce que signifie exactement chaque terme.
- **[PROTOCOL.md](./PROTOCOL.md)** — le format réseau, si vous implémentez face à lui.
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** — compiler, tester et proposer un correctif.

---

> Ceci est la version française de [USAGE.md](./USAGE.md). En cas de divergence, la version anglaise fait foi.
