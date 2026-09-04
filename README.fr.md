<h1 align="center">Lantunnel</h1>

<p align="center">
  <strong>Votre réseau privé, où que vous travailliez.</strong><br>
  Atteignez les machines et les services de vos propres réseaux locaux depuis n'importe où :
  pair à pair en priorité, chiffré de bout en bout, sans ouverture de ports ni URL publique.
</p>

<p align="center">
  <a href="https://github.com/lantunnel/lantunnel/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/lantunnel/lantunnel/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://lantunnel.app/"><img alt="Website" src="https://img.shields.io/badge/website-lantunnel.app-2563eb"></a>
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/badge/license-Apache--2.0-blue"></a>
  <img alt="Rust" src="https://img.shields.io/badge/rust-1.89%2B-orange">
  <img alt="Platforms" src="https://img.shields.io/badge/platforms-macOS%20%7C%20Windows%20%7C%20Linux%20%7C%20Android%20%7C%20iOS-lightgrey">
</p>

<p align="center">
  <a href="https://lantunnel.app/">Site</a> ·
  <a href="https://lantunnel.app/download">Téléchargements</a> ·
  <a href="./docs/USAGE.fr.md">Guide d'utilisation</a> ·
  <a href="./CONTEXT.md">Architecture</a> ·
  <a href="./docs/PROTOCOL.md">Protocole</a>
</p>

<p align="center">
  <a href="./README.md">English</a> ·
  <a href="./README.zh-CN.md">简体中文</a> ·
  <a href="./README.zh-TW.md">繁體中文</a> ·
  <a href="./README.ja.md">日本語</a> ·
  <a href="./README.es.md">Español</a> ·
  <a href="./README.de.md">Deutsch</a> ·
  <b>Français</b>
</p>

---

Le NAS est à la maison. La machine avec le GPU est au bureau. Votre instance `ollama` tourne sur le poste que vous avez laissé allumé en partant. Toutes derrière un NAT, et aucune n'a sa place sur l'internet public.

Lantunnel réunit ces machines dans un petit maillage privé — un **Tunnel** — auquel n'accèdent que les personnes à qui vous avez remis un profil. Les Peers se trouvent et communiquent **directement** dès que le réseau le permet. Sinon, ils basculent sur un **relais chiffré** via une Gateway qui transporte un texte chiffré qu'elle ne peut pas lire elle-même. Dans les deux cas : rien n'est publié, aucun port de box n'est ouvert, et rien n'est déchiffré en chemin.

> ### 🚀 Pas envie d'héberger une Gateway ? Ce n'est pas nécessaire.
>
> **[lantunnel.app](https://lantunnel.app/)** offre à chaque compte un **Tunnel gratuit permanent** : trafic pair à pair illimité, nombre illimité d'appareils LAN derrière chaque Client, et 5 Go de relais chiffré par mois pour les cas où la liaison directe échoue. Créez un Tunnel, téléchargez le Client, importez le profil — c'est tout. Ni serveur, ni certificat, ni DNS.
>
> Et si vous préférez héberger la Gateway vous-même, tout est dans ce dépôt, sous licence Apache-2.0, sans aucun décompte.
>
> **[→ Créez votre Tunnel gratuit](https://lantunnel.app/)**

---

## Ce que vous obtenez

| | |
|---|---|
| **La liaison directe d'abord** | Chaque nouveau flux tente d'abord une connexion QUIC pair à pair avec perçage de NAT en UDP. Le relais est la solution de repli, pas le chemin par défaut. |
| **Chiffrement de bout en bout** | Les données relayées sont scellées en XChaCha20-Poly1305 avec des clés issues d'un échange X25519 entre les deux Peers. La Gateway relaie des octets qu'elle ne peut pas déchiffrer. |
| **Aucune redirection de port** | Les Peers sortent d'eux-mêmes. Rien sur votre LAN n'a besoin de règle entrante, d'IP publique ni de nom d'hôte. |
| **Tout le LAN à portée** | Un Peer peut publier les sous-réseaux privés auxquels il est rattaché : un seul Client sur le réseau rend le NAS, l'imprimante et le tableau de bord interne accessibles au reste du Tunnel. |
| **Le contrôle d'accès vous appartient** | Chaque Client décide de ce qu'il expose. La politique réside sur la machine de destination — jamais sur la Gateway, jamais sur un serveur. |
| **Un binaire, avec ou sans interface** | `lantunnel-client` ouvre une fenêtre par défaut et exécute exactement le même runtime avec `--headless` sur un serveur. |
| **Partout** | macOS, Windows, Linux, Android et iOS. |

### Ce que les gens en font vraiment

- **Jeu et streaming multimédia** — Sunshine/Moonlight, Jellyfin ou Plex depuis la machine restée à la maison.
- **IA privée et outils de développement** — Ollama, Open WebUI, une API interne, un environnement de préproduction, une base de données qui ne doit jamais quitter le LAN.
- **Services domestiques et de bureau** — NAS, Home Assistant, caméras, tableaux de bord internes, SSH.

## Comment ça marche

```
┌─────────────────────── un Tunnel ───────────────────────┐
│                                                         │
│  ┌──────────────┐                     ┌──────────────┐  │
│  │    Peer A    │◀─── QUIC direct ───▶│    Peer B    │  │
│  │   portable   │      (préféré)      │  NAS maison  │  │
│  └───────┬──────┘                     └───────┬──────┘  │
│          │                                    │         │
└──────────┼────────────────────────────────────┼─────────┘
           │    ┌─────────────────────────┐     │
           └───▶│         Gateway         │◀────┘
       Relais   │  rendez-vous +          │  Relais
      chiffré   │  signalisation NAT +    │  chiffré
      (repli)   │  relais opaque          │  (repli)
                └─────────────────────────┘
                  ne voit que du chiffré
```

Trois composants, et c'est tout le système :

- **`lantunnel-client`** tourne sur chaque appareil qui rejoint le Tunnel. Il importe un profil `.peer` signé, se connecte à la Gateway et expose un proxy SOCKS5 en loopback, avec en option des routes natives pour que les applications ordinaires atteignent le Tunnel sans savoir qu'il existe.
- **`lantunnel-gateway`** est un point de rendez-vous et un signaleur pour la traversée de NAT. Elle admet un Tunnel parce qu'elle détient son fichier public `.scope`, aide les Peers à établir une liaison directe, et relaie des octets scellés quand ils n'y parviennent pas. Elle ne détient aucune clé privée de Peer et ne voit aucun texte en clair.
- **`lantunnel-admin`** crée le Tunnel hors ligne. Deux commandes : `init-tunnel` produit le fichier propriétaire et le scope public destiné à la Gateway, `add-peer` émet un profil signé par appareil. Il ne communique avec rien.

L'identité est signée, pas partagée. Pas de mot de passe de Tunnel, pas de secret de groupe, pas de jeton porteur : chaque Peer détient sa propre clé Ed25519, en prouve la possession à chaque rattachement, et cette clé ne quitte jamais la machine qui l'a générée.

📖 **[Architecture et concepts →](./CONTEXT.md)**  ·  📐 **[Protocole réseau →](./docs/PROTOCOL.md)**

## Démarrage rapide

### La voie rapide — Gateway hébergée

1. Créez votre Tunnel gratuit sur **[lantunnel.app](https://lantunnel.app/)**.
2. Ajoutez un Peer par appareil et téléchargez son profil `.peer`.
3. Installez le Client depuis **[lantunnel.app/download](https://lantunnel.app/download)** et importez le profil.

C'est fait. Pointez une application vers `127.0.0.1:1080`, ou activez le routage natif et utilisez directement les adresses LAN.

### La voie autonome — votre Gateway, vos règles

```bash
# 1. Créez le Tunnel hors ligne. Cette étape ne touche pas au réseau.
lantunnel-admin init-tunnel \
  --gateway-transport quic \
  --gateway-host gw.example.com \
  --gateway-port 8443
#   → <tunnel-id>.tunnel   à garder précieusement : la clé de signature du Tunnel
#   → <tunnel-id>.scope    public ; la Gateway n'a besoin que de ce fichier

# 2. Émettez un profil par appareil.
lantunnel-admin add-peer --tunnel <tunnel-id>.tunnel --name laptop --output laptop.peer
lantunnel-admin add-peer --tunnel <tunnel-id>.tunnel --name nas    --output nas.peer

# 3. Installez le scope public sur l'hôte de la Gateway, puis démarrez-la.
mkdir -p state/scopes.d && cp <tunnel-id>.scope state/scopes.d/
lantunnel-gateway --config configs/gateway.yaml

# 4. Sur chaque appareil, importez son propre profil et connectez-vous.
lantunnel-client tunnel import ./laptop.peer
lantunnel-client                          # interface graphique
lantunnel-client connect '<tunnel_id>'    # même runtime, sans fenêtre
```

Un profil par appareil — un `.peer` n'est pas fait pour être recopié d'une machine à l'autre.

📘 **[Guide complet — installation, publication de LAN, règles d'accès, serveurs, mobile, dépannage →](./docs/USAGE.fr.md)**

## Contenu du dépôt

Tout le nécessaire pour faire tourner Lantunnel vous-même, sous Apache-2.0 :

| Chemin | Contenu |
|---|---|
| `apps/lantunnel-client` | Le Client. Interface Tauri et runtime headless dans un même binaire. |
| `apps/lantunnel-gateway` | La Gateway. |
| `apps/lantunnel-admin` | Provisionnement hors ligne : `init-tunnel`, `add-peer`. |
| `apps/android-proxy` | Application Android (VpnService). |
| `apps/ios-proxy` | Application iOS (NetworkExtension). |
| `crates/tp-*` | Implémentation partagée : protocole, transports, proxies, P2P, moteurs Gateway et Client. |
| `docs/PROTOCOL.md` | Format réseau normatif. |
| `CONTEXT.md` | Architecture et vocabulaire. |
| `docs/USAGE.fr.md` | Comment s'en servir concrètement. |

La plateforme hébergée sur lantunnel.app — comptes, facturation, flotte de Gateways gérées — est un service distinct à source fermée qui ne fait **pas** partie de ce dépôt. Rien ici n'en dépend, et une installation auto-hébergée ne la contacte jamais.

## Compiler depuis les sources

Il vous faut Rust 1.89 ou plus récent, `protoc` pour le transport gRPC, et Node pour le frontend du Client.

```bash
# Gateway et outil de provisionnement
cargo build --release -p lantunnel-gateway
cargo build --release -p lantunnel-admin

# Client (compilez le frontend d'abord)
npm --prefix apps/lantunnel-client/frontend ci
npm --prefix apps/lantunnel-client/frontend run build
cargo build --release -p lantunnel-client
```

Sous Linux, le Client se lie à webkit2gtk, appindicator et rsvg ; la liste exacte des paquets `-dev` figure dans [`.github/workflows/ci.yml`](./.github/workflows/ci.yml).

Les vérifications, ainsi qu'une recette de bout en bout à trois Peers qui valide chaque paire dirigée TCP et UDP d'abord en liaison directe, puis en relais chiffré :

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
tests/e2e/v2_docker/run.sh
```

## Compatibilité

Les Peers, les Gateways et les profils doivent provenir de la même lignée 2.0.x : le format réseau ne se négocie pas d'une version à l'autre. Vous venez d'une installation 1.x ? Ses profils ne sont pas importables ; créez-en de nouveaux avec `lantunnel-admin`.

## Contribuer

Les issues et les pull requests sont les bienvenues — voyez [CONTRIBUTING.md](./CONTRIBUTING.md) pour la compilation, les tests et le style. Vous avez trouvé une faille ? Signalez-la en privé selon [SECURITY.md](./SECURITY.md), pas dans une issue publique.

## Licence

Apache License 2.0 — voir [LICENSE](./LICENSE) et [NOTICE](./NOTICE).

---

> Ceci est la version française du [README.md](./README.md). En cas de divergence, la version anglaise fait foi.

<p align="center">
  <strong>Passez l'étape installation.</strong> Un Tunnel gratuit permanent, du trafic direct illimité, des Gateways gérées prêtes à l'emploi.<br>
  <a href="https://lantunnel.app/"><strong>Commencez sur lantunnel.app →</strong></a>
</p>
