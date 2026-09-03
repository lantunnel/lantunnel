# Vendored Sources

These sources were vendored for the Android `tun2socks` JNI build.

- `hev-socks5-tunnel`: `https://github.com/heiher/hev-socks5-tunnel.git` at `13d7392241edeb5170c2516fca8aad804fa98cf0`
- `third-part/hev-task-system`: `https://github.com/heiher/hev-task-system.git` at `dfad1ebde3302f796afd5140aeef62c0e7c744fe`
- `third-part/yaml`: `https://github.com/heiher/yaml.git` at `9e7614d4310df7af71d593bb82672837d66e7657`
- `third-part/lwip`: `https://github.com/heiher/lwip.git` at `280142f4e25edd60320da92fd85263ee18eb37ee`
- `src/core`: `https://github.com/heiher/hev-socks5-core.git` at `e82fec1234859b5d3b8458dda497614b560ef181`

Equivalent refresh commands:

```sh
git clone --depth 1 https://github.com/heiher/hev-socks5-tunnel.git apps/android-proxy/app/src/main/jni/hev-socks5-tunnel
git clone --depth 1 https://github.com/heiher/hev-task-system.git apps/android-proxy/app/src/main/jni/hev-socks5-tunnel/third-part/hev-task-system
git clone --depth 1 https://github.com/heiher/yaml.git apps/android-proxy/app/src/main/jni/hev-socks5-tunnel/third-part/yaml
git clone --depth 1 https://github.com/heiher/lwip.git apps/android-proxy/app/src/main/jni/hev-socks5-tunnel/third-part/lwip
git clone --depth 1 https://github.com/heiher/hev-socks5-core.git apps/android-proxy/app/src/main/jni/hev-socks5-tunnel/src/core
find apps/android-proxy/app/src/main/jni/hev-socks5-tunnel -name .git -type d -prune -exec rm -rf {} +
```
