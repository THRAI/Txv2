# QEMU user networking through Clash TUN and Alpine TLS trust

**Date:** 2026-08-06

## Question

How does an RV Alpine Guest behind QEMU user networking interact with the
host's Clash/Mihomo configuration, and which Guest state/files determine
whether Git HTTPS passes TLS verification?

## Findings

- QEMU `-netdev user` creates ordinary host-side outbound connections on
  behalf of the Guest. It does not interpret Clash profiles or automatically
  consume the desktop HTTP proxy setting.
- Mihomo TUN with `auto-route` changes host routing so ordinary process traffic,
  normally including QEMU, enters TUN unless interface/UID/address exclusions,
  routing rules, or firewalls say otherwise. Thus “TUN on works, TUN off fails”
  is host-routing evidence, not a QEMU invariant.
- The Guest still independently needs a valid interface address, default route,
  and resolver. Host connectivity does not manufacture those Guest settings.
- In the documented manual `init=/bin/sh` path, QEMU `-netdev user` provides
  the DHCP server side but does not start a client inside the Guest. BusyBox
  `udhcpc` obtains the lease and `default.script` applies its address, route,
  and DNS. Current kernel sources do not consume `tx.net.mode`, so that token
  is not required by this manual command.
- TLS verification is userspace Git HTTPS/libcurl behavior above txKernel's
  DNS/TCP path. It requires a plausible Guest clock, hostname match, complete
  server chain, and a trusted CA anchor.
- The inspected RV mother image has an empty `/etc/resolv.conf`, a 212585-byte
  `/etc/ssl/certs/ca-certificates.crt`, and `/etc/ssl/cert.pem` pointing to that
  bundle. APK records `ca-certificates-bundle 20250619-r0`.
- This image does not contain `update-ca-certificates`, `openssl`, `curl`,
  `/usr/share/ca-certificates`, or `/usr/local/share/ca-certificates`. Generic
  Alpine custom-CA instructions therefore do not apply without first extending
  the image. The existing bundle is sufficient for the already verified public
  GitHub HTTPS path.
- Ordinary Clash TUN forwards encrypted TLS and normally does not require a
  Clash root CA inside the Guest. A custom CA is relevant only when an
  explicitly trusted intermediary performs TLS interception/re-signing.

## Sources

- QEMU user networking:
  <https://www.qemu.org/docs/master/system/devices/net.html#using-the-user-mode-network-stack>
- Mihomo TUN routing:
  <https://wiki.metacubex.one/en/config/inbound/tun/>
- curl TLS certificate verification:
  <https://curl.se/docs/sslcerts.html>
- Git `http.sslCAInfo` and verification configuration:
  <https://git-scm.com/docs/git-config/2.44.3.html#Documentation/git-config.txt-httpsslCAInfo>

## Verification

- Read-only `debugfs stat` against
  `local-images/alpine-linux-riscv64-ext4fs.img` confirmed the resolver, CA
  bundle, symlink, missing management paths/tools, and Git binary.
- Read-only inspection of `/lib/apk/db/installed` confirmed
  `ca-certificates-bundle 20250619-r0` for `riscv64`.
- A repository-wide literal search found `tx.net.mode` only in xtask scenario
  rendering/tests and progress documents, not in current kernel consumers.
- Existing 2026-08-06 QEMU evidence already proves DHCP, DNS, UTC/CA presence,
  and real `git ls-remote` to GitHub; this task changes documentation only and
  does not rerun the network witness.

## Next

If HTTPS fails, classify it by layer before changing configuration: IP and
route, DNS, clock, CA path/trust chain, then Git/proxy settings. Preserve TLS
verification. Extend the image with a full CA-management path only when a
specific trusted custom CA is genuinely required.

## Blockers

None for the already verified public GitHub HTTPS path. Custom CA installation
is intentionally not claimed ready in the current minimal image.
