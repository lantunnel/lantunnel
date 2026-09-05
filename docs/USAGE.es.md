# Uso de Lantunnel

Una guía práctica: conectarte, llegar a tus máquinas y decidir tú quién llega a las tuyas.

¿Es tu primer contacto con el proyecto? Empieza por el [README](../README.es.md). ¿Buscas el diseño que hay detrás? [CONTEXT.md](../CONTEXT.md).

[English](./USAGE.md) ·
[简体中文](./USAGE.zh-CN.md) ·
[繁體中文](./USAGE.zh-TW.md) ·
[日本語](./USAGE.ja.md) ·
**Español** ·
[Deutsch](./USAGE.de.md) ·
[Français](./USAGE.fr.md)

**Contenido**

1. [La idea en un minuto](#la-idea-en-un-minuto)
2. [Elige un camino](#elige-un-camino)
3. [Camino A — Gateway alojado (lo más rápido)](#camino-a--gateway-alojado-lo-más-rápido)
4. [Camino B — Gateway propio](#camino-b--gateway-propio)
5. [Cómo llegar a las cosas](#cómo-llegar-a-las-cosas)
6. [Compartir una LAN entera](#compartir-una-lan-entera)
7. [Decidir quién llega a ti](#decidir-quién-llega-a-ti)
8. [En un servidor (headless)](#en-un-servidor-headless)
9. [Móviles](#móviles)
10. [Referencia de comandos](#referencia-de-comandos)
11. [Referencia de ajustes](#referencia-de-ajustes)
12. [Dónde están los archivos](#dónde-están-los-archivos)
13. [Resolución de problemas](#resolución-de-problemas)

---

## La idea en un minuto

Un **Tunnel** es una pequeña red privada de máquinas que confían entre sí. Cada máquina que forma parte de él es un **Peer**, y cada Peer guarda un **perfil `.peer`** firmado: su identidad, su clave privada y cómo encontrar el Gateway.

En cuanto dos Peers están en el mismo Tunnel, hablan directamente siempre que la red lo permita. Cuando no lo permite, recurren a un relay a través del **Gateway**, que reenvía bytes sellados que no puede leer. El Gateway es un punto de encuentro, no un intermediario.

Tres cosas que nunca vas a necesitar: una IP pública en tu LAN, un puerto abierto en el router o una contraseña compartida.

## Elige un camino

|  | **Gateway alojado** | **Gateway propio** |
|---|---|---|
| Qué ejecutas | Solo el Client | El Client y tu propio Gateway |
| Qué necesitas | Una cuenta en [lantunnel.app](https://lantunnel.app/) | Una máquina con dirección pública |
| Tiempo de puesta en marcha | Minutos | Unos 20 minutos |
| Relay | 5 GB al mes gratis; por encima, medido | El tuyo, sin medición |
| P2P directo | Ilimitado | Ilimitado |

Ambos usan el mismo Client y el mismo protocolo. Puedes empezar con el alojado y mudarte después, o incluso mantener los dos, porque un Tunnel es independiente de cualquier cuenta.

---

## Camino A — Gateway alojado (lo más rápido)

**[lantunnel.app](https://lantunnel.app/)** se encarga de la flota de Gateways por ti. Cada cuenta incluye un Tunnel gratuito permanente: tráfico punto a punto ilimitado, dispositivos LAN ilimitados detrás de cada Client y 5 GB al mes de relay cifrado para cuando la conexión directa no salga.

1. **Crea un Tunnel** — regístrate en [lantunnel.app](https://lantunnel.app/) y crea tu Tunnel gratuito. Sin dirección de Gateway, sin certificados y sin DNS que configurar.
2. **Añade un Peer por dispositivo** — uno para el portátil, otro para el NAS, otro para el sobremesa. Descarga cada perfil `.peer`.
3. **Instala el Client** — desde [lantunnel.app/download](https://lantunnel.app/download) o compilándolo desde este repositorio.
4. **Importa y conecta:**

   ```bash
   lantunnel-client tunnel import ./laptop.peer
   lantunnel-client                       # abre la interfaz; conecta desde ahí
   ```

Un perfil gestionado solo lleva la URL de la plataforma. Al conectar, el Client pregunta en qué Gateway está ahora su Tunnel, firma la petición con su propia clave y recibe los datos de conexión. Si cambias de Gateway, no hay que tocar nada en tus dispositivos.

Puedes saltar directamente a [Cómo llegar a las cosas](#cómo-llegar-a-las-cosas).

---

## Camino B — Gateway propio

Todo lo que viene a continuación está en este repositorio bajo Apache-2.0. Nada contacta con lantunnel.app.

Mantén separados estos dos roles de máquina:

- **Host del Gateway:** la máquina pública contiene el binario del Gateway, el par de claves TLS y los archivos públicos `.scope`.
- **Equipo de confianza del propietario:** contiene `lantunnel-admin`, el archivo privado `.tunnel` del propietario y los archivos `.peer` de cada instalación antes de transferir cada uno a su Client.

Nunca instales `lantunnel-admin` ni guardes archivos `.tunnel` o `.peer` en el host público del Gateway.

### Qué necesitas

- Una máquina accesible desde internet: un VPS de 5 dólares sobra. El Gateway se dedica sobre todo a la señalización, y el relay solo transporta lo que la vía directa no consigue llevar.
- Dos reglas de entrada en ella: tu **puerto de datos** (TCP o UDP, según el transporte) y el **puerto UDP de mapeo** elegido (`8444` de forma predeterminada).
- Una dirección IPv4 o IPv6 pública fija. La vía principal genera la identidad TLS; los nombres de host y certificados de confianza pública usan la vía manual avanzada.

### 1. Compila (o descarga) los binarios

```bash
# Host del Gateway
cargo build --release -p lantunnel-gateway

# Equipo de confianza del propietario
cargo build --release -p lantunnel-admin

# Ejecuta esto en cada shell de compilación después del comando correspondiente.
export PATH="$PWD/target/release:$PATH"
```

Instala [Lantunnel Client](https://lantunnel.app/download) en cada dispositivo Peer. Si prefieres compilar el Client, sigue los comandos del frontend y de Rust de [Compilar desde el código](../README.es.md#compilar-desde-el-código).

### 2. Inicializa el Gateway independiente

En el host del Gateway, ejecuta el inicializador sin conexión con su IP pública fija:

```bash
lantunnel-gateway init --public-ip <PUBLIC_IP>
```

El comando no contacta con lantunnel.app ni con ninguna otra plataforma. De forma predeterminada configura el listener de datos QUIC en UDP `8443`, el listener de mapeo en UDP `8444` y `configs/gateway.yaml`. Usa `--transport`, `--data-port`, `--mapping-port` o `--config` para cambiar el transporte, el puerto de datos, el puerto de mapeo o la ruta de configuración.

Genera `configs/gateway.yaml`, `certs/server.crt`, `certs/server.key` y `state/scopes.d`. En Linux y macOS, los directorios son exclusivos del propietario (permisos `0700`), y la configuración, el certificado y la clave usan permisos `0600`.

Con el mismo archivo `--config`, la repetición exacta de `init`, la validación y el arranque funcionan desde cualquier directorio de trabajo.

`certs/server.crt` es el certificado público autofirmado cuyo SAN contiene exactamente esa IP. `certs/server.key` permanece exclusivamente en el host del Gateway. Repetir exactamente el mismo comando conserva byte por byte la clave, el certificado y la configuración.

Si, con la misma ruta de configuración, la IP, el transporte, el puerto de datos o el puerto de mapeo no coinciden con el estado existente, `init` se niega a sustituir nada. Otro archivo de configuración situado en la misma raíz de despliegue puede reutilizar el mismo certificado compatible.

**Vía manual avanzada: nombre de host o certificado de una CA pública.** `init --public-ip` no gestiona este caso. Guarda la cadena de certificados y la clave bajo `certs/` como dos archivos normales distintos (no enlaces simbólicos), asigna permisos `0600` a ambos y crea `configs/gateway.yaml` siguiendo el paso 5. Para un certificado autofirmado de nombre de host puedes seguir usando:

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

### 3. Crea el Tunnel sin conexión

Copia desde el host del Gateway únicamente el archivo público `certs/server.crt` a la máquina de confianza del propietario; nunca copies `certs/server.key`. Guarda allí la copia pública como `./server.crt`. `lantunnel-admin` nunca habla con la red, y el archivo `.tunnel` que produce es la clave privada de firma del Tunnel.

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

Esto escribe dos archivos nombrados según el Tunnel ID generado:

| Archivo | Para quién | Contiene |
|---|---|---|
| `<tunnel-id>.tunnel` | **Solo para ti.** Permisos `0600`. | La clave privada de firma del Tunnel. Si la pierdes no podrás emitir más Peers; si se filtra, otro podrá hacerlo por ti. |
| `<tunnel-id>.scope` | Para el Gateway. Público. | El Tunnel ID y la clave *pública* de firma, nada más. No puede emitir Peers ni leer tráfico. |

Opciones de `init-tunnel`:

- `--gateway-transport quic | websocket | grpc` — QUIC es la opción por defecto y la única con flujos independientes por conexión. WebSocket y gRPC están pensados para redes que bloquean UDP.
- `--gateway-host` y/o `--gateway-ip` — si indicas ambos, la conexión se establece mediante la IP y el nombre de host se usa como nombre de servidor TLS.
- `--gateway-mapping-port` — el puerto UDP de mapeo del Gateway. El valor predeterminado es `8444` y debe coincidir con `lantunnel-gateway init --mapping-port` o `gateway.mapping_probe_port`.
- `--gateway-cert` — el PEM que se va a fijar. Omítelo si el Gateway usa un certificado de confianza pública.

El ejemplo fija el certificado generado por `init`. En la vía manual avanzada con un certificado de nombre de host de confianza pública, usa `--gateway-host` y omite `--gateway-cert` para que las renovaciones normales del certificado no exijan nuevos perfiles de Peer.

### 4. Emite un perfil por dispositivo

```bash
lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name laptop --output ./provision/laptop.peer

lantunnel-admin add-peer --tunnel ./provision/<tunnel-id>.tunnel \
  --name nas --output ./provision/nas.peer
```

Cada `add-peer` asigna una **Overlay IP** del rango `198.18.0.0/16`, genera un par de claves nuevo, firma la pertenencia y actualiza el archivo de propietario de forma atómica.

> **Un `.peer` por dispositivo.** Copiar un perfil a una segunda máquina no clona un Peer: las dos instancias se pelean por la misma identidad y el Gateway rechaza a la que pierde.

Opciones útiles: `--overlay-ip` para fijar una dirección y `--replicas` para permitir a ese Peer más de una conexión de transporte simultánea.

### 5. Arranca el Gateway

Copia **solo el scope público** al directorio que `init` generó en el host del Gateway:

```bash
cp /ruta/a/<tunnel-id>.scope state/scopes.d/
```

Valida la configuración generada y después arranca el Gateway:

```bash
lantunnel-gateway --config configs/gateway.yaml --check-config
lantunnel-gateway --config configs/gateway.yaml
```

La configuración generada guarda las rutas de los archivos de ejecución como rutas absolutas. Aquí, `<DEPLOYMENT_ROOT>` es el directorio persistente de despliegue que contiene el archivo de configuración (o su directorio `configs/`) y los directorios generados `certs/` y `state/`. En la vía manual avanzada, los datos de conexión deben coincidir con los pasados a `init-tunnel`:

```yaml
gateway:
  listen_addr: "0.0.0.0:8443"     # debe coincidir con --gateway-port
  transport_type: "quic"          # debe coincidir con --gateway-transport
  tls_cert: "<DEPLOYMENT_ROOT>/certs/server.crt"
  tls_key: "<DEPLOYMENT_ROOT>/certs/server.key"
  scopes_dir: "<DEPLOYMENT_ROOT>/state/scopes.d"    # deja aquí los archivos .scope
  mapping_probe_port: 8444        # UDP; valor predeterminado configurable
```

El Gateway abre él mismo el puerto UDP de mapeo elegido: no hay un segundo proceso que arrancar. Un listener de datos QUIC no puede usar el mismo puerto que el listener UDP de mapeo; WebSocket y gRPC sí pueden reutilizar el número porque sus listeners de datos usan TCP.

Puedes editar `gateway.mapping_probe_port` después de `init` y antes de emitir perfiles de Peer. Pasa el mismo valor a `lantunnel-admin init-tunnel --gateway-mapping-port` y abre ese puerto UDP en el firewall.

Si cambias el puerto más tarde, abre el nuevo puerto UDP, actualiza `gateway.mapping_probe_port` en el YAML y reinicia el Gateway.

Cambiar solo el YAML rompe las sondas de mapeo de los perfiles de Peer existentes.

Actualiza también `static_gateway.mapping_port` en el `.tunnel` existente y `bootstrap.mapping_port` en cada `.peer` existente. Vuelve a importar esos perfiles y reconecta los Clients.

El Tunnel ID, el `.scope` instalado y las firmas de pertenencia de los Peers siguen siendo válidos; no hacen falta un Tunnel o Scope nuevos ni volver a firmar.

Si no conservas el `.peer` original, usa `add-peer` con el mismo `.tunnel` para crear una identidad Peer nueva.

Para añadir otro Tunnel más adelante basta con dejar otro `.scope` en `scopes_dir`. Hay unidades de systemd de ejemplo en [`scripts/remote/`](../scripts/remote/).

### 6. Conecta los dispositivos

```bash
lantunnel-client tunnel import ./laptop.peer
lantunnel-client tunnel list          # comprobación; nunca imprime claves privadas
lantunnel-client                      # interfaz gráfica
lantunnel-client connect <tunnel-id>  # o en modo headless
```

---

## Cómo llegar a las cosas

Una vez conectado tienes dos formas de meter tráfico en el Tunnel.

### 1. El proxy SOCKS5 local — siempre activo

Todo Client conectado expone un proxy SOCKS5 en **`127.0.0.1:1080`**, solo en loopback y sin autenticación. No la necesita: está atado al loopback y cada petición que pasa por él se autoriza contra la política del Peer *de destino*.

```bash
curl --socks5-hostname 127.0.0.1:1080 http://198.18.0.7:8096      # Jellyfin en un Peer
curl --socks5-hostname 127.0.0.1:1080 http://192.168.1.50         # NAS en la LAN de un Peer
```

Los navegadores, `ssh -o ProxyCommand`, Docker y la mayoría de herramientas de línea de comandos aceptan un proxy SOCKS5 directamente. Si el `1080` está ocupado, muévelo con `--local-socks5-listen 127.0.0.1:1081`.

Con el Client conectado, el panel de ajustes del escritorio copia un fragmento YAML de Clash listo para pegar apuntando a este listener.

### 2. Enrutado nativo — todas las aplicaciones, sin configurar nada

Al activar el enrutado nativo, la máquina instala rutas reales para el Tunnel, de modo que *cualquier* aplicación llega a los Peers por dirección sin saber que Lantunnel existe.

```bash
lantunnel-client --desktop-network-mode lan_routes_tun \
                 --lan-route 192.168.1.0/24
```

También puedes cambiar el modo de red y añadir las rutas desde la interfaz de escritorio. En un móvil no es una opción: el servicio VPN es la única manera de alcanzar el tráfico de otras aplicaciones, así que se aplica siempre.

**Tunnel First** decide qué pasa cuando una ruta remota del Tunnel se solapa con la red en la que estás físicamente. Desactivado (por defecto) gana tu LAN local; activado gana el Tunnel, algo útil cuando el wifi de la cafetería también usa `192.168.1.0/24`. En ambos casos, el Gateway, el canal de control, el DNS y los destinos que tú mismo exportas siguen protegidos en sus rutas nativas.

### ¿Qué dirección uso?

| Para llegar a | Usa |
|---|---|
| Un servicio en la propia máquina del Peer remoto | Su **Overlay IP** (`198.18.x.y`) con el puerto del servicio. `lantunnel-client tunnel list` la imprime en JSON y la interfaz también la muestra. |
| Un dispositivo en la LAN del Peer remoto | La **dirección LAN real** de ese dispositivo, por ejemplo `192.168.1.50`. |

Por defecto, un puerto del Overlay se mapea a `127.0.0.1` en el mismo puerto de la máquina de destino.

---

## Compartir una LAN entera

Un Peer puede anunciar las subredes privadas en las que está. A partir de ahí, el resto de Peers llega a *cualquier cosa* en esas subredes a través de él —el NAS, la impresora, la interfaz web del switch— sin instalar nada en esos dispositivos.

Hay dos fuentes independientes, ambas activadas por defecto en la interfaz:

- **Exportar la LAN actual** (`auto_export_current_lan`, activado por defecto) publica las redes privadas a las que esta máquina está conectada en cada momento, y las vuelve a calcular en cada escaneo de interfaces. Lleva el portátil de casa a la oficina y la exportación le acompaña.
- **Exportaciones escritas a mano** (`exported_lans`) son los prefijos que indicas tú.

Desactivar el interruptor automático retira únicamente lo que él añadió; tu lista manual queda intacta.

Solo se aceptan prefijos IPv4 de RFC1918. Se rechazan las rutas por defecto, los rangos públicos, loopback, link-local, multicast y cualquier cosa que se solape con el pool del Overlay.

**Exportar crea alcance, no permiso.** Un Peer remoto sigue teniendo que pasar la [política de acceso](#decidir-quién-llega-a-ti) del Client que exporta para cada destino.

Si dos Peers exportan el mismo prefijo, cada Client elige el primero que vio y pasa al siguiente cuando el último camino de ese se cae. Es una decisión por Client y no se guarda, así que es normal que dos de tus máquinas elijan exportadores distintos.

---

## Decidir quién llega a ti

La **política de acceso del Client** es la única ACL de Lantunnel, y vive en la máquina a la que se llega. Ni en el Gateway ni en un servidor. La selección de ruta decide *hacia dónde* enviar; tu Client decide por su cuenta si atiende o no.

Comportamiento por defecto: una política vacía significa que **cualquier Peer con un perfil de tu Tunnel puede llegar a ti**. Conseguir ese perfil ya exigía que tú lo emitieras, de modo que una segunda barrera encima no añadía ningún límite: solo hacía que las instalaciones recién hechas resultaran inalcanzables sin explicación. En cuanto escribes una regla Allow, esa pasa a ser la única entrada. **Deny se evalúa siempre primero y siempre gana.**

Configúrala en la interfaz de escritorio, o en `settings.json`:

```jsonc
{
  "client_access": {
    "allow": [
      // SSH a esta máquina
      { "target": { "type": "this_peer" }, "protocol": "tcp", "port": { "type": "exact", "value": 22 } },
      // Jellyfin en el NAS que tiene al lado
      { "target": { "type": "ip", "value": "192.168.1.50" }, "protocol": "tcp", "port": { "type": "exact", "value": 8096 } },
      // Cualquier puerto TCP de la subred de IoT
      { "target": { "type": "cidr", "value": "192.168.9.0/24" }, "protocol": "tcp", "port": { "type": "any" } }
    ],
    "deny": [
      // ...y el router nunca, diga lo que diga la lista Allow
      { "target": { "type": "ip", "value": "192.168.1.1" }, "protocol": "tcp", "port": { "type": "any" } }
    ]
  }
}
```

Los destinos son `this_peer`, `ip`, `cidr` o `host`. Los puertos son `any` o `exact`; no hay rangos. El orden de las reglas no significa nada: lo único que cuenta es que Deny gana a Allow. Una regla nunca nombra un Peer de origen: todos los miembros autenticados del Tunnel reciben la misma respuesta.

Para rechazarlo todo, deniega `0.0.0.0/0` y `::/0` en TCP y UDP. Es exactamente lo que escribe el botón «bloquear todo el tráfico entrante» de la interfaz, de modo que el archivo guardado coincide con lo que pediste.

---

## En un servidor (headless)

`--headless` (alias `--no-ui`) ejecuta exactamente el mismo runtime sin ventana, sin icono de bandeja y sin WebView: la misma lógica de reconexión, el mismo comportamiento de PeerLink y relay, las mismas superficies SOCKS5 y TUN.

```bash
lantunnel-client tunnel import /etc/lantunnel/nas.peer
lantunnel-client connect <tunnel-id>          # en primer plano, sin interfaz
lantunnel-client status --json                # desde otra terminal
lantunnel-client disconnect
```

Usar `--headless` a secas conecta el perfil marcado para conexión automática, así que la unidad de servicio no necesita ningún Tunnel ID:

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

El modo headless no tiene interfaz de ajustes, así que edita directamente `settings.json` en el directorio de configuración; consulta la [referencia de ajustes](#referencia-de-ajustes).

**En Windows**, las compilaciones de release usan el subsistema GUI, así que un arranque normal no abre ninguna consola y `cmd.exe` no espera al proceso. Cuando te importen la salida y el código de salida de un comando corto, usa `start /wait`:

```
start /wait "" "C:\Program Files\Lantunnel\lantunnel-client.exe" status --json
```

---

## Móviles

Android (`apps/android-proxy`, VpnService) e iOS (`apps/ios-proxy`, NetworkExtension) ejecutan el mismo núcleo en Rust a través de `tp-mobile-ffi`. Importa el perfil `.peer` escaneando su código QR o abriendo el archivo, y arranca la VPN.

En un móvil no hay conmutador de modo de red: el servicio VPN es la única forma de alcanzar el tráfico de otras aplicaciones, así que el enrutado nativo sigue siempre al runtime.

---

## Referencia de comandos

### `lantunnel-client`

```
lantunnel-client                          Abre la interfaz de escritorio
lantunnel-client connect <TUNNEL_ID>      Conecta un perfil importado, sin interfaz
lantunnel-client disconnect               Desconecta el Client en ejecución
lantunnel-client status --json            Imprime el estado en JSON
lantunnel-client tunnel import <FILE>     Importa un perfil .peer
lantunnel-client tunnel list              Lista los perfiles en JSON
```

`tunnel list` imprime el Tunnel ID, el Peer ID, la Overlay IP y el tipo de arranque de cada perfil importado. El material de clave privada no es serializable y nunca aparece.

| Opción | Significado |
|---|---|
| `--headless`, `--no-ui` | Ejecuta el runtime completo sin interfaz |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug`, `trace` |
| `--local-socks5-listen <ADDR>` | Cambia la dirección del listener SOCKS5 de loopback |
| `--desktop-network-mode <MODE>` | `socks5_only` o `lan_routes_tun` |
| `--lan-route <CIDR>` | Instala una ruta LAN nativa (repetible) |
| `--enable-lan-p2p` | Permite direcciones LAN como candidatas a la vía directa |
| `-V`, `--help` | Versión, ayuda |

Sustituciones por entorno: `LANTUNNEL_LOCAL_SOCKS5_LISTEN`, `LANTUNNEL_DESKTOP_NETWORK_MODE`, `LANTUNNEL_LAN_ROUTES`, `TUNNEL_PROXY_APP_CONFIG_DIR`.

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

Está pensado para funcionar sin conexión. Rechaza los enlaces simbólicos y no sobrescribe archivos existentes.

### `lantunnel-gateway`

```
lantunnel-gateway [--config <FILE>] [--check-config]       Ejecuta o valida el Gateway
lantunnel-gateway init --public-ip <PUBLIC_IP>             Inicializa sin conexión un Gateway independiente por IP
                       [--transport <quic|websocket|grpc>]
                       [--data-port <PORT>] [--mapping-port <PORT>]
                       [--config <FILE>]
lantunnel-gateway onboard --pairing <FILE>       Da de alta un Gateway gestionado
lantunnel-gateway mapping serve                  Reflector UDP de mapeo independiente
```

`init` funciona sin conexión y usa de forma predeterminada QUIC en UDP `8443` y mapeo en UDP `8444`; usa `--mapping-port` para elegir otro puerto de mapeo. `--config` usa `configs/gateway.yaml` por defecto. `mapping serve` existe para despliegues poco habituales; un Gateway normal abre su propio socket de mapeo y no lo necesita.

El alta gestionada debe comenzar en un directorio de trabajo nuevo y accesible solo por su propietario, para que pueda escribir allí su configuración de ejecución. Sigue la [guía de instalación de un Gateway conectado a la plataforma](https://lantunnel.app/docs/installation#platform-connected) o usa la misma secuencia segura:

```bash
mkdir -m 700 lantunnel-gateway-state
mv /path/to/downloaded-pairing.yaml lantunnel-gateway-state/pairing.yaml
chmod 600 lantunnel-gateway-state/pairing.yaml
cd lantunnel-gateway-state
lantunnel-gateway onboard --pairing pairing.yaml
```

---

## Referencia de ajustes

`settings.json`, en el directorio de configuración del Client. Todas las claves son opcionales.

| Clave | Por defecto | Significado |
|---|---|---|
| `auto_start` | `false` | Arrancar al iniciar sesión |
| `auto_connect` | `false` | Conectar al arrancar |
| `local_proxy_enabled` | `true` | Levantar el listener SOCKS5 local |
| `local_socks5_listen` | `"127.0.0.1:1080"` | Su dirección (solo loopback) |
| `desktop_network_mode` | `"socks5_only"` | O `"lan_routes_tun"` para rutas nativas |
| `lan_routes` | `[]` | Rutas nativas a instalar en modo `lan_routes_tun` |
| `tunnel_first` | `false` | Deja que las rutas del Tunnel ganen a las locales solapadas |
| `exported_lans` | `[]` | Prefijos privados que publica este Peer |
| `auto_export_current_lan` | `true` | Publicar también las redes en las que está esta máquina |
| `client_access` | abierto | La ACL: ver [más arriba](#decidir-quién-llega-a-ti) |
| `p2p_allow_lan_candidates` | `false` | Ofrecer direcciones LAN como candidatas a la vía directa |
| `log_level` | `"info"` | Nivel de log del Client |

Las claves desconocidas se rechazan en lugar de ignorarse, así que una errata sale a la luz en vez de quedarse sin efecto en silencio.

---

## Dónde están los archivos

| Qué | Ruta |
|---|---|
| Configuración del Client, perfiles importados, secretos | `~/.lantunnel/app/` (se puede cambiar con `TUNNEL_PROXY_APP_CONFIG_DIR`) |
| Ajustes del Client | `~/.lantunnel/app/settings.json` |
| Configuración del Gateway | `configs/gateway.yaml` (o la indicada con `--config`) |
| Admisión de Tunnels en el Gateway | `state/scopes.d/*.scope` |
| Registro de uso de relay del Gateway | `state/relay-usage.wal` |
| Archivo de propietario del Tunnel | Donde lo dejara `init-tunnel --output-dir`; haz copia de seguridad |

La clave privada importada se guarda en un archivo que el Client crea con permisos solo para su propietario; nunca se escribe en un log, nunca se envía a un Gateway y nunca sale de la máquina.

---

## Resolución de problemas

**El Client no conecta.**
Comprueba que el Gateway esté en marcha y que su puerto de datos se alcance desde fuera (`nc -z gw.example.com 8443`, o `nc -zu` para QUIC). Después confirma que el `.scope` del Tunnel está en el `scopes_dir` del Gateway: sin él, el Gateway no tiene motivo alguno para admitirte.

**«Peer already attached», o un Client que se desconecta una y otra vez.**
Hay dos Clients usando el mismo `.peer`. Emite un segundo perfil con `add-peer`: un perfil es la identidad de un dispositivo, no una credencial compartida.

**Todo funciona, pero siempre por relay.**
Mira los contadores de tráfico de la interfaz: separan directo de relay. Un NAT simétrico en ambos extremos puede echar abajo la perforación. Si los dos Peers están en la misma LAN, añade `--enable-lan-p2p` para ofrecer las direcciones locales como candidatas. Verifica también que el puerto UDP de mapeo configurado (`8444` de forma predeterminada) llega al Gateway; sin la sonda de mapeo, ninguno de los dos Peers descubre su mapeo público.

**El directo funciona y el relay no (o al revés).**
Son caminos independientes. El relay necesita el puerto de datos del Gateway; el directo necesita que el UDP fluya entre los Peers. Prueba uno cada vez.

**Un servicio remoto rechaza la conexión.**
Lo está rechazando la política de acceso del Client de destino: revisa `client_access` *en esa máquina*, no en la tuya. Un resultado `NotAuthorized` es definitivo y nunca se reintenta contra otro Peer.

**Una LAN exportada no responde.**
El Client que exporta tiene que estar conectado a esa red en ese momento para que la exportación esté lista: un prefijo configurado solo se publica cuando coincide exactamente con uno conectado. Después de eso, comprueba que la política de acceso de ese Client permita el destino y el puerto concretos.

**Versiones que no coinciden.**
Peers, Gateways y perfiles deben ser de la misma línea 2.0.x. El formato de red no se negocia entre versiones, y un despliegue mixto falla cerrado.

**Necesito más detalle.**
`--log-level debug` en el Client y `log.level: debug` en la configuración del Gateway. Los logs nunca contienen claves privadas, contenido de perfiles ni claves de sesión.

---

## Por dónde seguir

- **[lantunnel.app](https://lantunnel.app/)** — Tunnel gratuito, Gateways gestionados, descargas y guías para streaming de juegos, herramientas de IA privadas y servicios domésticos.
- **[CONTEXT.md](../CONTEXT.md)** — cómo encajan las piezas y qué significa exactamente cada término.
- **[PROTOCOL.md](./PROTOCOL.md)** — el formato de red, si vas a implementar contra él.
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** — compilar, probar y enviar un parche.

---

> Esta es la versión en español de [USAGE.md](./USAGE.md). Si ambas difieren, la versión en inglés prevalece.
