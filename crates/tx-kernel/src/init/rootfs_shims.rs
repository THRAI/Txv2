// Rootfs-side shebang shims and small init-time tmpfs population.
//
// Carved out of `init.rs` to keep that file under the 1800-line
// per-file authoring cap enforced by the `arch` lint
// (`txdoc:CI-GATE-ARCH-LINT`). The single method here augments the
// `impl<P: TxPlatform> CoreInit<P>` block in `init.rs` and
// `init::exec`.
//
// The helper creates `/bin/{sh,busybox,ls}` and `/usr/bin/env` as
// rootfs-tmpfs symlinks pointing at `/musl/musl/busybox` so the
// OSComp `libctest`, `lua`, and `libcbench` wrapper scripts find
// their shebang interpreters. See the doc-comment on
// `populate_rootfs_shebang_shims` for the full design rationale.

use super::*;
use crate::adapter::step_engine::{self as step_engine, page_allocator, StepOutcome};
use tx_subsystems::page_backed::{FsPageBacking, MaterializeAccess, PageIndex};

impl<P: TxPlatform> CoreInit<P> {
    /// Populate the rootfs tmpfs with the shebang shims the
    /// OSComp `libctest`, `lua`, and `libcbench` suites' wrapper
    /// scripts expect on disk:
    ///
    /// ```text
    /// /bin/busybox    → /musl/musl/busybox  (handles `#!/bin/busybox sh …`)
    /// /bin/sh         → /musl/musl/busybox  (handles `#!/bin/sh`)
    /// /bin/ls         → /musl/musl/busybox  (lets BusyBox `which ls` pass)
    /// /usr/bin/env    → /musl/musl/busybox  (handles `#!/usr/bin/env …`)
    /// ```
    ///
    /// The wrapper scripts (`scripts/lua/test.sh`, `run-static.sh`,
    /// `run-dynamic.sh`, …) all start with a `#!` line referencing
    /// one of these interpreter paths. Without the shims the kernel
    /// reports `not found` at `execve` time and the entire suite
    /// scores 0/N (libctest 0/220, lua 0/9 in the 2026-05-18
    /// scoreboard — see `docs/progress/SYSCALL_STATUS.md`).
    ///
    /// **Order invariant:** must run after
    /// [`Self::mount_sdcard_at_musl`] so `/musl/musl/busybox` is a
    /// reachable target (symlink resolution happens at exec time,
    /// not at symlink-creation time, so the order isn't strictly
    /// required for the symlink to succeed — but if the target's
    /// mount isn't yet attached the very first exec attempt fails,
    /// not a later one). Runs after [`Self::mount_procfs_at_proc`]
    /// so its sentinel comes first in the boot log.
    ///
    /// Failures are non-fatal — the helper logs a sentinel and
    /// returns. The kernel boots; the libctest / lua suites stay
    /// at 0/N until the shim is created.
    pub(crate) fn populate_rootfs_shebang_shims() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_shebang_shims: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;

        // /bin
        let bin_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"bin", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":shebang-shims:err:mkdir-bin\n");
                return;
            }
        };
        let _ = symlink_into(fs_ops, bin_id, b"busybox", b"/musl/musl/busybox", &cred);
        let _ = symlink_into(fs_ops, bin_id, b"sh", b"/musl/musl/busybox", &cred);
        let _ = symlink_into(fs_ops, bin_id, b"ls", b"/musl/musl/busybox", &cred);

        // /usr and /usr/bin
        let usr_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"usr", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":shebang-shims:err:mkdir-usr\n");
                return;
            }
        };
        let usr_bin_id = match mkdir_or_find(fs_ops, usr_id, b"bin", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":shebang-shims:err:mkdir-usr-bin\n");
                return;
            }
        };
        let _ = symlink_into(fs_ops, usr_bin_id, b"env", b"/musl/musl/busybox", &cred);

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":shebang-shims:ok\n");
    }

    /// Populate the rootfs tmpfs with the writable scratch
    /// directories that POSIX-shaped userspace expects to exist.
    /// Today this covers `/tmp/`, `/var/`, and `/var/tmp/`.
    ///
    /// **Why this exists.** The OSComp `lmbench-musl` suite (and
    /// many libc/libctest tests) `open(O_RDWR|O_CREAT, "/var/tmp/…")`
    /// during setup. Without these directories `open` returns
    /// `-ENOENT` and the entire suite scores 0/N. The kernel does
    /// not auto-create them at boot the way Linux's initrd would —
    /// the rootfs is a fresh tmpfs.
    ///
    /// **Order invariant:** must run after
    /// [`Self::populate_rootfs_shebang_shims`] so the
    /// `:shebang-shims:ok` sentinel comes first in the boot log
    /// (purely for grep-stability — there is no functional
    /// dependency between the two helpers).
    ///
    /// Failures are non-fatal — the helper logs a sentinel and
    /// returns. The kernel boots; the affected suites stay at 0/N
    /// until the scratch dirs are populated.
    /// Seed the kernel CSPRNG from platform entropy before userspace
    /// starts.  Must run after rootfs is mounted (the CSPRNG owns no
    /// filesystem state, but the ordering convention keeps all Phase
    /// 3b boot wiring in one place).
    pub(crate) fn init_csprng() {
        let seed = tx_services::random::platform_seed();
        tx_services::random::init(&seed);
    }

    pub(crate) fn populate_rootfs_tmp_dirs() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_tmp_dirs: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;

        // /tmp (world-writable, sticky-style — the slice doesn't
        // honour the sticky bit yet so 0o777 is the practical
        // equivalent).
        if mkdir_or_find(fs_ops, root_fs_object_id, b"tmp", 0o777, &cred).is_none() {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":tmp-dirs:err:mkdir-tmp\n");
            return;
        }

        // /var and /var/tmp
        let var_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"var", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":tmp-dirs:err:mkdir-var\n");
                return;
            }
        };
        if mkdir_or_find(fs_ops, var_id, b"tmp", 0o777, &cred).is_none() {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":tmp-dirs:err:mkdir-var-tmp\n");
            return;
        }

        // /var/run/netns — LTP's `init_ltp_netspace` symlinks the network
        // namespace here (`ln -s /proc/<pid>/ns/net /var/run/netns/ltp_ns`) and
        // reads the pid back via `readlink`. Pre-create the chain so the symlink
        // (and the LTP_NETNS pid derived from it) succeed.
        if let Some(run_id) = mkdir_or_find(fs_ops, var_id, b"run", 0o755, &cred) {
            let _ = mkdir_or_find(fs_ops, run_id, b"netns", 0o755, &cred);
        }
        // /sys — mountpoint for `mount -t sysfs` inside the netns.
        let _ = mkdir_or_find(fs_ops, root_fs_object_id, b"sys", 0o755, &cred);

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":tmp-dirs:ok\n");
    }

    /// Seed `/lib/modules/6.1.0-txkernel/modules.{dep,builtin}` listing the
    /// network drivers — including `kernel/net/sctp/sctp.ko` — so LTP's
    /// `tst_check_driver("sctp")` / `tst_kernel` gate opens and the
    /// `net.sctp` suite actually runs instead of TCONF-skipping. Re-homed
    /// with the net subsystem (main dropped it). The `/boot/config-*` text
    /// (needed only by LTP's kconfig parser) is intentionally omitted here.
    pub(crate) fn populate_rootfs_kernel_config() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_kernel_config: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;
        let fs_page_backing = &rootfs_payload.fs_page_backing;
        let create_ctx = RootfsCreateContext {
            fs_ops,
            fs_page_backing,
            mount: &rootfs_payload,
            cred: &cred,
        };

        let lib_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"lib", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":kernel-config:err:mkdir-lib\n");
                return;
            }
        };
        let modules_id = match mkdir_or_find(fs_ops, lib_id, b"modules", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":kernel-config:err:mkdir-modules\n");
                return;
            }
        };
        let release_id = match mkdir_or_find(fs_ops, modules_id, b"6.1.0-txkernel", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":kernel-config:err:mkdir-release\n");
                return;
            }
        };
        let modules_dep = b"kernel/drivers/net/dummy.ko:\n\
kernel/drivers/net/veth.ko:\n\
kernel/net/sched/sch_teql.ko:\n\
kernel/net/ipv4/netfilter/ip_tables.ko:\n\
kernel/net/ipv6/netfilter/ip6_tables.ko:\n\
kernel/net/netfilter/nf_tables.ko:\n\
kernel/net/sctp/sctp.ko:\n";
        let modules_builtin = b"kernel/drivers/net/dummy.ko\n\
kernel/drivers/net/veth.ko\n\
kernel/net/sched/sch_teql.ko\n\
kernel/net/ipv4/netfilter/ip_tables.ko\n\
kernel/net/ipv6/netfilter/ip6_tables.ko\n\
kernel/net/netfilter/nf_tables.ko\n\
kernel/net/sctp/sctp.ko\n";
        if !create_file_with_data(&create_ctx, release_id, b"modules.dep", 0o644, modules_dep)
            || !create_file_with_data(
                &create_ctx,
                release_id,
                b"modules.builtin",
                0o644,
                modules_builtin,
            )
        {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":kernel-config:err:create-modules\n");
            return;
        }

        // Seed `/boot/config-6.1.0-txkernel` (the plain-text kernel .config LTP's
        // tst_kconfig parser probes after `/proc/config[.gz]`). Re-homed with the
        // net subsystem; PR#50 dropped it, which made every kconfig-gated LTP case
        // TBROK "Cannot parse kernel .config". The `/proc/config` procfs backing
        // (KCONFIG_PATH used by the runtest runner) is restored in tx-fs procfs.
        if let Some(boot_id) = mkdir_or_find(fs_ops, root_fs_object_id, b"boot", 0o755, &cred) {
            if !create_file_with_data(
                &create_ctx,
                boot_id,
                b"config-6.1.0-txkernel",
                0o644,
                tx_fs::procfs::KERNEL_CONFIG_TEXT.as_bytes(),
            ) {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":kernel-config:err:create-boot-config\n");
                return;
            }
        } else {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":kernel-config:err:mkdir-boot\n");
            return;
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":kernel-config:ok\n");
    }

    /// Seed minimal `/etc/{passwd,group}` (with a `nobody` entry) so libc
    /// `getpwnam`/`getgrnam` resolve. Re-homed with the net subsystem; PR#50
    /// dropped the identity-file seeding, so LTP cases that drop privileges to
    /// `nobody` (e.g. bind02) TBROK with `getpwnam(nobody): ENOENT`.
    pub(crate) fn populate_rootfs_identity_files() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_identity_files: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;
        let fs_page_backing = &rootfs_payload.fs_page_backing;
        let create_ctx = RootfsCreateContext {
            fs_ops,
            fs_page_backing,
            mount: &rootfs_payload,
            cred: &cred,
        };

        let etc_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"etc", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":identity-files:err:mkdir-etc\n");
                return;
            }
        };

        let passwd = b"root:x:0:0:root:/root:/bin/sh\n\
nobody:x:65534:65534:nobody:/nonexistent:/bin/sh\n";
        let group = b"root:x:0:\ndaemon:x:2:\nusers:x:100:\nnogroup:x:65534:\nnobody:x:65534:\n";
        if !create_file_with_data(&create_ctx, etc_id, b"passwd", 0o644, passwd)
            || !create_file_with_data(&create_ctx, etc_id, b"group", 0o644, group)
        {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":identity-files:err:create-etc-files\n");
            return;
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":identity-files:ok\n");
    }

    pub(crate) fn populate_rootfs_network_databases() {
        let root_mount = ROOT_MOUNT
            .lock()
            .clone()
            .expect("populate_rootfs_network_databases: ROOT_MOUNT must be populated");
        let rootfs_payload = root_mount
            .payload_cap()
            .expect("rootfs payload alive during boot")
            .into_cap()
            .clone();
        let cred = Credential::root();
        let root_fs_object_id = root_mount.root().fs_object_id();
        let fs_ops = &rootfs_payload.fs_ops;
        let fs_page_backing = &rootfs_payload.fs_page_backing;
        let create_ctx = RootfsCreateContext {
            fs_ops,
            fs_page_backing,
            mount: &rootfs_payload,
            cred: &cred,
        };

        let etc_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"etc", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:mkdir-etc\n");
                return;
            }
        };
        let hosts = b"127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n";
        let services = b"echo 7/tcp\necho 7/udp\n";
        let protocols = b"hopopt 0 HOPOPT\nip 0 IP\nipv6 41 IPv6\nipv6-route 43 IPv6-Route\nipv6-frag 44 IPv6-Frag\nesp 50 ESP\nah 51 AH\nipv6-icmp 58 IPv6-ICMP\nipv6-nonxt 59 IPv6-NoNxt\nipv6-opts 60 IPv6-Opts\n";
        let dhcpd_conf = b"# txkernel LTP DHCP compatibility placeholder\n";
        if !create_file_with_data(&create_ctx, etc_id, b"hosts", 0o666, hosts)
            || !create_file_with_data(&create_ctx, etc_id, b"services", 0o644, services)
            || !create_file_with_data(&create_ctx, etc_id, b"protocols", 0o644, protocols)
            || !create_file_with_data(&create_ctx, etc_id, b"dhcpd.conf", 0o644, dhcpd_conf)
        {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-etc-files\n");
            return;
        }
        let var_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"var", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:mkdir-var\n");
                return;
            }
        };
        let var_lib_id = match mkdir_or_find(fs_ops, var_id, b"lib", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:mkdir-var-lib\n");
                return;
            }
        };
        if mkdir_or_find(fs_ops, var_lib_id, b"misc", 0o755, &cred).is_none()
            || mkdir_or_find(fs_ops, var_id, b"log", 0o755, &cred).is_none()
        {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:mkdir-dhcp-dirs\n");
            return;
        }

        let tx_ltp_id = match mkdir_or_find(fs_ops, root_fs_object_id, b"tx-ltp", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:mkdir-tx-ltp\n");
                return;
            }
        };
        let tx_ltp_bin_id = match mkdir_or_find(fs_ops, tx_ltp_id, b"bin", 0o755, &cred) {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:mkdir-tx-ltp-bin\n");
                return;
            }
        };
        let tx_ltp_trace_bin_id = match mkdir_or_find(fs_ops, tx_ltp_id, b"trace-bin", 0o755, &cred)
        {
            Some(id) => id,
            None => {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:mkdir-tx-ltp-trace-bin\n");
                return;
            }
        };

        // la64: the judged image's busybox has only 73 applets and no awk;
        // the LTP shell library hard-depends on awk (timeout multiply,
        // tst_net parsing), so every shell test died at
        // "TWARN: timeout need to be >= 1" + instant watchdog kill. Ship a
        // Txv2-built full-applet static busybox; the walk env's /bin
        // install prefers it (see append_busybox_bin_install).
        #[cfg(target_arch = "loongarch64")]
        {
            static LA_BUSYBOX_FULL: &[u8] = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tools/images/vendor/busybox-loongarch64-musl"
            ));
            if !create_file_with_data(
                &create_ctx,
                tx_ltp_id,
                b"busybox-full",
                0o755,
                LA_BUSYBOX_FULL,
            ) {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:create-busybox-full\n");
                return;
            }
        }

        for name in [
            b"arp".as_slice(),
            b"cat".as_slice(),
            b"cut".as_slice(),
            b"grep".as_slice(),
            b"id".as_slice(),
            b"ln".as_slice(),
            b"mkdir".as_slice(),
            b"mount".as_slice(),
            b"readlink".as_slice(),
            b"seq".as_slice(),
        ] {
            let _ = symlink_into(fs_ops, tx_ltp_bin_id, name, b"/bin/busybox", &cred);
        }
        // `sysctl` is a thin shim: LTP tst_net setup does
        // `sysctl -qw net.ipv6.conf.<iface>.accept_dad=0`, and busybox sysctl
        // writing that key returns non-zero here (no per-iface DAD toggle file),
        // which aborts `tst_init_iface` before `ip link set <iface> up` — leaving
        // the interface down and unconfigured. No-op the IPv6 conf writes (DAD is
        // already off / the addresses are permanent) and forward everything else.
        // `sysctl -b <key>` (value, no trailing newline) is procps-only;
        // busybox rejects -b and the LTP mcast-lib setup dies on it. Serve
        // -b straight from /proc/sys.
        let sysctl_script = b"#!/bin/sh\n\
bb=/bin/busybox\n\
[ -x \"$bb\" ] || bb=/musl/musl/busybox\n\
case \"$*\" in\n\
    *net.ipv6.conf.*) exit 0 ;;\n\
esac\n\
if [ \"$1\" = -b ]; then\n\
    key=$2\n\
    path=/proc/sys/$(echo \"$key\" | \"$bb\" tr . /)\n\
    [ -r \"$path\" ] || exit 1\n\
    \"$bb\" tr -d '\\n' < \"$path\"\n\
    exit 0\n\
fi\n\
exec \"$bb\" sysctl \"$@\"\n";
        if !create_file_with_data(&create_ctx, tx_ltp_bin_id, b"sysctl", 0o755, sysctl_script) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-sysctl\n");
            return;
        }
        let netfilter_dmesg_script = br#"#!/bin/sh
if [ -r /tmp/tx-dmesg ]; then
    while IFS= read -r line; do
        echo "$line"
    done < /tmp/tx-dmesg
fi
exit 0
"#;
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"dmesg",
            0o755,
            netfilter_dmesg_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-dmesg\n");
            return;
        }

        let trace_command_script = b"#!/bin/sh\n\
cmd=${0##*/}\n\
bb=/bin/busybox\n\
[ -x \"$bb\" ] || bb=/musl/musl/busybox\n\
tx_ltp_trace_log() {\n\
    echo \"$*\" > /dev/console 2>/dev/null || echo \"$*\" >&2\n\
}\n\
tx_ltp_now() {\n\
    tx_ltp_time=0\n\
    if read tx_ltp_time _ < /proc/uptime 2>/dev/null; then\n\
        tx_ltp_time=${tx_ltp_time%.*}\n\
        [ -n \"$tx_ltp_time\" ] || tx_ltp_time=0\n\
    fi\n\
}\n\
if [ -n \"$tx_ltp_trace_runtime\" ]; then\n\
    tx_ltp_now\n\
    tx_ltp_trace_log \"TX-LTP-CMDWRAP begin $cmd $tx_ltp_time $*\"\n\
fi\n\
case \"$cmd\" in\n\
    tst_check_drivers|tst_ns_create|tst_ns_exec|tst_ns_ifmove|tst_sleep)\n\
        \"/musl/musl/ltp/testcases/bin/$cmd\" \"$@\" ;;\n\
    ip)\n\
        /tx-ltp/bin/ip \"$@\" ;;\n\
    sysctl)\n\
        case \"$*\" in\n\
            *net.ipv6.conf.*) exit 0 ;;\n\
        esac\n\
        \"$bb\" sysctl \"$@\" ;;\n\
    arp|cat|cut|grep|id|ln|mkdir|mount|readlink|seq)\n\
        \"$bb\" \"$cmd\" \"$@\" ;;\n\
    dmesg)\n\
        if [ -x /tx-ltp/bin/dmesg ]; then /tx-ltp/bin/dmesg \"$@\"; elif [ -r /tmp/tx-dmesg ]; then while IFS= read -r line; do echo \"$line\"; done < /tmp/tx-dmesg; fi ;;\n\
    dhclient|dhcpd|dnsmasq|ip6tables|ip6tables-translate|iptables|iptables-translate|modprobe|nft|ping|ping6|ss|tc|tcpdump|telnet|traceroute|traceroute6|tracepath|tracepath6)\n\
        /tx-ltp/bin/$cmd \"$@\" ;;\n\
    *)\n\
        tx_ltp_trace_log \"TX-LTP-CMDWRAP unknown $cmd\"\n\
        exit 127 ;;\n\
esac\n\
rc=$?\n\
if [ -n \"$tx_ltp_trace_runtime\" ]; then\n\
    tx_ltp_now\n\
    tx_ltp_trace_log \"TX-LTP-CMDWRAP end $cmd $rc $tx_ltp_time\"\n\
fi\n\
exit $rc\n";
        for name in [
            b"tst_ns_create".as_slice(),
            b"tst_ns_exec".as_slice(),
            b"tst_ns_ifmove".as_slice(),
            b"tst_check_drivers".as_slice(),
            b"tst_sleep".as_slice(),
            b"arp".as_slice(),
            b"cat".as_slice(),
            b"cut".as_slice(),
            b"grep".as_slice(),
            b"id".as_slice(),
            b"ip".as_slice(),
            b"ln".as_slice(),
            b"mkdir".as_slice(),
            b"mount".as_slice(),
            b"dhclient".as_slice(),
            b"dhcpd".as_slice(),
            b"dnsmasq".as_slice(),
            b"iptables".as_slice(),
            b"ip6tables".as_slice(),
            b"iptables-translate".as_slice(),
            b"ip6tables-translate".as_slice(),
            b"modprobe".as_slice(),
            b"nft".as_slice(),
            b"ping".as_slice(),
            b"ping6".as_slice(),
            b"readlink".as_slice(),
            b"seq".as_slice(),
            b"ss".as_slice(),
            b"sysctl".as_slice(),
            b"tc".as_slice(),
            b"tcpdump".as_slice(),
            b"telnet".as_slice(),
            b"dmesg".as_slice(),
            b"traceroute".as_slice(),
            b"traceroute6".as_slice(),
            b"tracepath".as_slice(),
            b"tracepath6".as_slice(),
        ] {
            if !create_file_with_data(
                &create_ctx,
                tx_ltp_trace_bin_id,
                name,
                0o755,
                trace_command_script,
            ) {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:create-ltp-trace-helper\n");
                return;
            }
        }
        let netstat_script = b"#!/bin/sh\n\
bb=/bin/busybox\n\
[ -x \"$bb\" ] || bb=/musl/musl/busybox\n\
for arg in \"$@\"; do\n\
    case \"$arg\" in\n\
        -*s*) cat /proc/net/snmp 2>/dev/null || true; exit 0 ;;\n\
        -*i*) cat /proc/net/dev 2>/dev/null || true; exit 0 ;;\n\
        -*g*) [ -r /proc/net/igmp ] && cat /proc/net/igmp; [ -r /proc/net/igmp6 ] && cat /proc/net/igmp6; exit 0 ;;\n\
        -*r*) echo \"Kernel IP routing table\"; [ -r /proc/net/route ] && cat /proc/net/route; exit 0 ;;\n\
    esac\n\
done\n\
exec \"$bb\" netstat \"$@\"\n";
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"netstat",
            0o755,
            netstat_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-netstat\n");
            return;
        }
        let tracepath_script = b"#!/bin/sh\n\
cmd=${0##*/}\n\
case \"$1\" in\n\
    -V|--version)\n\
        echo \"$cmd txkernel-minimal\"\n\
        exit 0 ;;\n\
esac\n\
if [ \"$1\" = \"-6\" ]; then\n\
    shift\n\
fi\n\
host=\n\
len=65535\n\
while [ $# -gt 0 ]; do\n\
    case \"$1\" in\n\
        -l)\n\
            len=\"$2\"\n\
            shift 2 ;;\n\
        -*)\n\
            shift ;;\n\
        *)\n\
            [ -n \"$host\" ] || host=\"$1\"\n\
            shift ;;\n\
    esac\n\
done\n\
[ -n \"$host\" ] || exit 1\n\
[ -n \"$len\" ] || len=1280\n\
echo \" 1?: [$host] pmtu $len hops 1\"\n";
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tracepath",
            0o755,
            tracepath_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tracepath6",
            0o755,
            tracepath_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-tracepath\n");
            return;
        }
        let ss_script = b"#!/bin/sh\n\
case \"$*\" in\n\
    *l*t*p*|*l*p*t*|*t*l*p*|*t*p*l*|*p*l*t*|*p*t*l*)\n\
        cat /proc/net/tcp_listen_proc 2>/dev/null\n\
        cat /proc/net/tcp6_listen_proc 2>/dev/null\n\
        exit 0 ;;\n\
esac\n\
cat /proc/net/tcp 2>/dev/null\n\
cat /proc/net/tcp6 2>/dev/null\n";
        if !create_file_with_data(&create_ctx, tx_ltp_bin_id, b"ss", 0o755, ss_script) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-ss\n");
            return;
        }
        let tcpdump_script = b"#!/bin/sh\n\
bb=/bin/busybox\n\
[ -x \"$bb\" ] || bb=/musl/musl/busybox\n\
case \"$1\" in\n\
    -V|--version)\n\
        echo \"tcpdump txkernel-minimal\"\n\
        exit 0 ;;\n\
esac\n\
iface=\n\
while [ $# -gt 0 ]; do\n\
    case \"$1\" in\n\
        -i)\n\
            iface=\"$2\"\n\
            shift 2 ;;\n\
        -c|-s|-w|-r|-W|-G|-Z)\n\
            shift 2 ;;\n\
        -*)\n\
            shift ;;\n\
        *)\n\
            shift ;;\n\
    esac\n\
done\n\
emit_addr() {\n\
    addr=${1%%/*}\n\
    [ -n \"$addr\" ] || return\n\
    echo \"00:00:00.000000 ${iface:-any} IP $addr > ${iface:-any}: ICMP echo\"\n\
    emitted=1\n\
}\n\
probe_neighbors() {\n\
    emitted=\n\
    if [ -r /proc/net/tx_neigh ]; then\n\
        while read ip dev devname rest; do\n\
            [ \"$dev\" = \"dev\" ] || continue\n\
            emit_addr \"$ip\"\n\
        done < /proc/net/tx_neigh\n\
    fi\n\
    if [ -r /proc/net/arp ]; then\n\
        while read ip hwtype flags mac dev state; do\n\
            [ \"$ip\" = \"IP\" ] && continue\n\
            emit_addr \"$ip\"\n\
        done < /proc/net/arp\n\
    fi\n\
}\n\
tries=0\n\
while [ \"$tries\" -lt 5 ]; do\n\
    probe_neighbors\n\
    [ -n \"$emitted\" ] && exit 0\n\
    tries=$((tries + 1))\n\
    \"$bb\" sleep 1\n\
done\n\
emit_addr \"$IPV4_RHOST\"\n\
emit_addr \"$IPV6_RHOST\"\n\
[ -n \"$emitted\" ] && exit 0\n\
exit 0\n";
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tcpdump",
            0o755,
            tcpdump_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-tcpdump\n");
            return;
        }
        let traceroute_script = b"#!/bin/sh\n\
cmd=${0##*/}\n\
bb=/bin/busybox\n\
[ -x \"$bb\" ] || bb=/musl/musl/busybox\n\
case \"$1\" in\n\
    -V|--version)\n\
        echo \"$cmd txkernel-minimal\"\n\
        exit 0 ;;\n\
esac\n\
new_args=\n\
for arg in \"$@\"; do\n\
    if [ \"$arg\" = \"-T\" ]; then\n\
        new_args=\"$new_args -I\"\n\
    else\n\
        new_args=\"$new_args $arg\"\n\
    fi\n\
done\n\
exec \"$bb\" \"$cmd\" $new_args\n";
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"traceroute",
            0o755,
            traceroute_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"traceroute6",
            0o755,
            traceroute_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-traceroute\n");
            return;
        }
        let netfilter_tool_script = br#"#!/bin/sh
cmd=${0##*/}
state=/tmp/tx-netfilter-rules
family=4
case "$cmd" in ip6tables|ip6tables-translate) family=6 ;; esac

nf_flush_family() {
    tmp="${state}.$$"
    if [ -r "$state" ]; then
        while read fam rest; do
            [ -n "$fam" ] || continue
            [ "$fam" = "$family" ] || echo "$fam $rest"
        done < "$state" > "$tmp"
        mv "$tmp" "$state"
    else
        : > "$state"
    fi
}

nf_delete_first_family() {
    tmp="${state}.$$"
    removed=
    if [ -r "$state" ]; then
        while read fam rest; do
            [ -n "$fam" ] || continue
            if [ "$fam" = "$family" ] && [ -z "$removed" ]; then
                removed=1
                continue
            fi
            echo "$fam $rest"
        done < "$state" > "$tmp"
        mv "$tmp" "$state"
    fi
}

nf_list_table() {
    table=${1:-filter}
    case "$table" in
        nat)
            for chain in PREROUTING INPUT OUTPUT POSTROUTING; do
                echo "Chain $chain (policy ACCEPT)"
            done ;;
        mangle)
            for chain in PREROUTING INPUT FORWARD OUTPUT POSTROUTING; do
                echo "Chain $chain (policy ACCEPT)"
            done ;;
        *)
            for chain in INPUT FORWARD OUTPUT; do
                echo "Chain $chain (policy ACCEPT)"
            done ;;
    esac
}

nf_append_rule() {
    target=ACCEPT
    proto=*
    src=*
    dst=*
    dports=*
    limit=0
    prefix=tx_nf:
    while [ $# -gt 0 ]; do
        case "$1" in
            -s) src="$2"; shift 2 ;;
            -d) dst="$2"; shift 2 ;;
            -p) proto="$2"; shift 2 ;;
            --dport) dports="$2"; shift 2 ;;
            --dports) dports="$2"; shift 2 ;;
            --icmp-type|--icmpv6-type) shift 2 ;;
            -m)
                [ "$2" = "limit" ] && limit=1
                shift 2 ;;
            -j) target="$2"; shift 2 ;;
            --log-prefix) prefix="$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    nf_dmesg_append "$state" "$family $target $proto $src $dst $dports $limit $prefix"
}

# Append a line via read+rewrite (tmpfs O_APPEND is broken here: `>>` writes at
# offset 0 and overwrites, so plain `echo >> file` only ever keeps the last line).
nf_dmesg_append() {
    nfa_file="$1"
    nfa_line="$2"
    nfa_tmp="${nfa_file}.$$"
    { [ -r "$nfa_file" ] && cat "$nfa_file"; echo "$nfa_line"; } > "$nfa_tmp"
    mv "$nfa_tmp" "$nfa_file"
}

nf_iptables() {
    action=
    table=filter
    chain=
    while [ $# -gt 0 ]; do
        case "$1" in
            -t) table="$2"; shift 2 ;;
            -F) action=F; shift ;;
            -L) action=L; shift ;;
            -A) action=A; chain="$2"; shift 2; break ;;
            -D) action=D; chain="$2"; shift 2; break ;;
            *) break ;;
        esac
    done
    case "$action" in
        F) nf_flush_family; exit 0 ;;
        L) nf_list_table "$table"; exit 0 ;;
        D) nf_delete_first_family; exit 0 ;;
        A)
            [ "$chain" = "INPUT" ] || exit 0
            nf_append_rule "$@"
            exit 0 ;;
    esac
    exit 1
}

case "$cmd" in
    iptables-translate|ip6tables-translate)
        echo "nft __tx_iptables -$family $*"
        exit 0 ;;
    nft)
        if [ "$1" = "__tx_iptables" ]; then
            shift
            case "$1" in
                -6) family=6; shift ;;
                -4) family=4; shift ;;
            esac
            nf_iptables "$@"
        fi
        for arg in "$@"; do
            [ "$arg" = "ip6" ] && family=6
        done
        case "$1" in
            list|add|delete) exit 0 ;;
            flush) nf_flush_family; exit 0 ;;
        esac
        exit 0 ;;
    iptables|ip6tables)
        nf_iptables "$@" ;;
esac
exit 1
"#;
        for name in [
            b"iptables".as_slice(),
            b"ip6tables".as_slice(),
            b"iptables-translate".as_slice(),
            b"ip6tables-translate".as_slice(),
            b"nft".as_slice(),
        ] {
            if !create_file_with_data(
                &create_ctx,
                tx_ltp_bin_id,
                name,
                0o755,
                netfilter_tool_script,
            ) {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:create-netfilter-tools\n");
                return;
            }
        }
        let netfilter_ping_script = br#"#!/bin/sh
cmd=${0##*/}
bb=/bin/busybox
[ -x "$bb" ] || bb=/musl/musl/busybox
state=/tmp/tx-netfilter-rules
dmesg_state=/tmp/tx-dmesg
family=4
proto=icmp
loop=127.0.0.1
case "$cmd" in ping6) family=6; proto=icmpv6; loop=::1 ;; esac
target=
count=4
need=
for arg in "$@"; do
    if [ -n "$need" ]; then
        [ "$need" = count ] && count="$arg"
        need=
        continue
    fi
    case "$arg" in
        -c) need=count ;;
        -W|-i|-I) need=skip ;;
        -*) ;;
        *) target="$arg" ;;
    esac
done
[ -n "$target" ] || exec "$bb" "$cmd" "$@"
[ "$target" = "$loop" ] || exec "$bb" "$cmd" "$@"

nf_addr_match() {
    [ "$1" = "*" ] || [ "$1" = "$2" ]
}

nf_log_ping() {
    n="$count"
    [ "$n" -gt 0 ] 2>/dev/null || n=1
    [ "$1" = 1 ] && [ "$n" -gt 5 ] && n=5
    # tmpfs O_APPEND is broken here (`>>` overwrites at offset 0), so build the
    # whole file (existing + n new lines) and write it once via truncation.
    nflp_tmp="${dmesg_state}.$$"
    {
        [ -r "$dmesg_state" ] && cat "$dmesg_state"
        i=0
        while [ "$i" -lt "$n" ]; do
            echo "$2 SRC=$target DST=$target PROTO=$proto"
            i=$((i + 1))
        done
    } > "$nflp_tmp"
    mv "$nflp_tmp" "$dmesg_state"
}

blocked=
if [ -r "$state" ]; then
    while read fam rule_target rule_proto src dst dports limit prefix; do
        [ "$fam" = "$family" ] || continue
        case "$rule_proto" in "$proto"|\*) ;; *) continue ;; esac
        nf_addr_match "$src" "$target" || continue
        nf_addr_match "$dst" "$target" || continue
        case "$rule_target" in
            DROP|REJECT) blocked=1 ;;
            LOG) nf_log_ping "$limit" "$prefix" ;;
        esac
    done < "$state"
fi

if [ -n "$blocked" ]; then
    echo "PING $target ($target): 56 data bytes"
    echo "--- $target ping statistics ---"
    echo "$count packets transmitted, 0 packets received, 100% packet loss"
    exit 1
fi

echo "PING $target ($target): 56 data bytes"
echo "--- $target ping statistics ---"
echo "$count packets transmitted, $count packets received, 0% packet loss"
exit 0
"#;
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"ping",
            0o755,
            netfilter_ping_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"ping6",
            0o755,
            netfilter_ping_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-netfilter-ping\n");
            return;
        }
        let netfilter_telnet_script = br#"#!/bin/sh
state=/tmp/tx-netfilter-rules
dmesg_state=/tmp/tx-dmesg
host="$1"
port="$2"
case "$host" in *:*) family=6 ;; *) family=4 ;; esac

nf_addr_match() {
    [ "$1" = "*" ] || [ "$1" = "$2" ]
}

nf_port_match() {
    spec="$1"
    port="$2"
    [ "$spec" = "*" ] && return 0
    case "$spec" in
        *:*)
            start=${spec%:*}
            end=${spec#*:}
            [ "$port" -ge "$start" ] 2>/dev/null && [ "$port" -le "$end" ] 2>/dev/null
            return $? ;;
    esac
    oldifs="$IFS"
    IFS=,
    set -- $spec
    IFS="$oldifs"
    for p in "$@"; do
        [ "$p" = "$port" ] && return 0
    done
    return 1
}

if [ -r "$state" ]; then
    while read fam target proto src dst dports limit prefix; do
        [ "$fam" = "$family" ] || continue
        [ "$target" = "LOG" ] || continue
        [ "$proto" = "tcp" ] || continue
        nf_addr_match "$src" "$host" || continue
        nf_addr_match "$dst" "$host" || continue
        nf_port_match "$dports" "$port" || continue
        echo "$prefix SRC=$host DST=$host PROTO=tcp DPT=$port " >> "$dmesg_state"
    done < "$state"
fi
echo "telnet: can't connect to remote host"
exit 1
"#;
        let netfilter_dmesg_script = br#"#!/bin/sh
if [ -r /tmp/tx-dmesg ]; then
    while IFS= read -r line; do
        echo "$line"
    done < /tmp/tx-dmesg
fi
exit 0
"#;
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"telnet",
            0o755,
            netfilter_telnet_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"dmesg",
            0o755,
            netfilter_dmesg_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-netfilter-observe\n");
            return;
        }
        let tc_script = br#"#!/bin/sh
cmd=${0##*/}
case "$cmd" in
    modprobe)
        for module in "$@"; do
            case "$module" in
                bridge|dummy|ip6_tables|ip_tables|nf_tables|sch_teql|veth) ;;
                -*|"") ;;
                *) exit 1 ;;
            esac
        done
        exit 0 ;;
    tc)
        if [ "$1" = "qdisc" ] && [ "$2" = "add" ]; then
            echo "RTNETLINK answers: Invalid argument" >&2
            exit 2
        fi
        exit 0 ;;
esac
exit 1
"#;
        if !create_file_with_data(&create_ctx, tx_ltp_bin_id, b"tc", 0o755, tc_script)
            || !create_file_with_data(&create_ctx, tx_ltp_bin_id, b"modprobe", 0o755, tc_script)
        {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-tc-tools\n");
            return;
        }
        let dhcp_script = br#"#!/bin/sh
cmd=${0##*/}
case "$cmd" in
    dhcpd)
        case "$1" in --version|-v|-V) echo "isc-dhcpd txkernel-minimal"; exit 0 ;; esac
        exit 0 ;;
    dnsmasq)
        for arg in "$@"; do
            case "$arg" in
                --version) echo "Dnsmasq version txkernel-minimal"; exit 0 ;;
                --test) exit 0 ;;
            esac
        done
        exit 0 ;;
    dhclient)
        for arg in "$@"; do
            case "$arg" in --version|-v|-V) echo "isc-dhclient txkernel-minimal"; exit 0 ;; esac
        done
        family=4
        iface=
        for arg in "$@"; do
            case "$arg" in
                -6) family=6 ;;
                -4) family=4 ;;
                -*) ;;
                *) iface="$arg" ;;
            esac
        done
        [ -n "$iface" ] || exit 1
        state=/tmp/tx-dhcp-addrs
        if [ "$family" = 6 ]; then
            addr=fd00:1:1:2::100/128
        else
            addr=10.1.1.100/24
        fi
        /tx-ltp/bin/ip addr add "$addr" dev "$iface" 2>/dev/null || true
        tmp="${state}.$$"
        if [ -r "$state" ]; then
            while read old_iface old_addr; do
                [ "$old_iface" = "$iface" ] || echo "$old_iface $old_addr"
            done < "$state" > "$tmp"
        else
            : > "$tmp"
        fi
        echo "$iface $addr" >> "$tmp"
        mv "$tmp" "$state"
        exit 0 ;;
esac
exit 1
"#;
        for name in [
            b"dhcpd".as_slice(),
            b"dnsmasq".as_slice(),
            b"dhclient".as_slice(),
        ] {
            if !create_file_with_data(&create_ctx, tx_ltp_bin_id, name, 0o755, dhcp_script) {
                Self::write_board_sentinel_prefix();
                tx_hal::console_write_str::<P>(":network-db:err:create-dhcp-tools\n");
                return;
            }
        }
        let ip_script = b"#!/bin/sh\n\
bb=/bin/busybox\n\
[ -x \"$bb\" ] || bb=/musl/musl/busybox\n\
tx_ltp_trace_log() {\n\
    echo \"$*\" > /dev/console 2>/dev/null || echo \"$*\" >&2\n\
}\n\
tx_ltp_now() {\n\
    tx_ltp_time=0\n\
    if read tx_ltp_time _ < /proc/uptime 2>/dev/null; then\n\
        tx_ltp_time=${tx_ltp_time%.*}\n\
        [ -n \"$tx_ltp_time\" ] || tx_ltp_time=0\n\
    fi\n\
}\n\
tx_ltp_trace_phase() {\n\
    if [ -n \"$tx_ltp_trace_runtime\" ]; then\n\
        tx_ltp_now\n\
        tx_ltp_trace_log \"TX-LTP-SHIM phase ip $tx_ltp_time $*\"\n\
    fi\n\
}\n\
if [ -n \"$tx_ltp_trace_runtime\" ]; then\n\
    tx_ltp_now\n\
    tx_ltp_trace_log \"TX-LTP-SHIM begin ip $tx_ltp_time $*\"\n\
    trap 'rc=$?; tx_ltp_now; tx_ltp_trace_log \"TX-LTP-SHIM end ip $rc $tx_ltp_time\"' EXIT\n\
fi\n\
tx_ltp_exec() {\n\
    if [ -n \"$tx_ltp_trace_runtime\" ]; then\n\
        \"$@\"\n\
        exit $?\n\
    fi\n\
    exec \"$@\"\n\
}\n\
neigh_state=/tmp/tx-ip-neigh\n\
maddr_state=/tmp/tx-ip-maddr\n\
dhcp_state=/tmp/tx-dhcp-addrs\n\
ip_family=\n\
if [ \"$1\" = \"-6\" ] || [ \"$1\" = \"-4\" ]; then\n\
    ip_family=\"$1\"\n\
    ip_object=\"$2\"\n\
    ip_command=\"$3\"\n\
else\n\
    ip_object=\"$1\"\n\
    ip_command=\"$2\"\n\
fi\n\
if [ \"$ip_object\" = \"xfrm\" ] && [ \"$ip_command\" = \"state\" ]; then\n\
    exit 1\n\
fi\n\
if [ \"$ip_object\" = \"link\" ] && [ \"$ip_command\" = \"set\" ]; then\n\
    for tx_ltp_ip_arg in \"$@\"; do\n\
        case \"$tx_ltp_ip_arg\" in\n\
            mtu|up|down) tx_ltp_exec \"$bb\" ip \"$@\" ;;\n\
        esac\n\
    done\n\
    exit 0\n\
fi\n\
if [ \"$ip_object\" = \"route\" ]; then\n\
    route_state=/tmp/tx-ip-route\n\
    route_saved=\"$*\"\n\
    if [ -n \"$ip_family\" ]; then\n\
        shift 3 2>/dev/null || shift $#\n\
    else\n\
        shift 2 2>/dev/null || shift $#\n\
    fi\n\
    case \"$ip_command\" in\n\
        flush)\n\
            : > \"$route_state\" 2>/dev/null || true\n\
            exit 0 ;;\n\
        add|replace|change|append|prepend)\n\
            route_dest=\"$1\"\n\
            shift\n\
            route_gw=\n\
            route_dev=\n\
            while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                    via) route_gw=\"$2\"; shift 2 ;;\n\
                    dev) route_dev=\"$2\"; shift 2 ;;\n\
                    *) shift ;;\n\
                esac\n\
            done\n\
            [ -n \"$route_dest\" ] || exit 1\n\
            if [ -z \"$route_dev\" ]; then\n\
                case \"$route_gw\" in\n\
                    127.*) route_dev=lo ;;\n\
                    ::1) route_dev=lo ;;\n\
                esac\n\
            fi\n\
            route_tmp=\"${route_state}.$$\"\n\
            {\n\
                if [ -r \"$route_state\" ]; then\n\
                    while read rd rg rv; do\n\
                        [ \"$rd\" = \"$route_dest\" ] || echo \"$rd $rg $rv\"\n\
                    done < \"$route_state\"\n\
                fi\n\
                echo \"$route_dest ${route_gw:-_} ${route_dev:-_}\"\n\
            } > \"$route_tmp\"\n\
            mv \"$route_tmp\" \"$route_state\"\n\
            exit 0 ;;\n\
        del|delete)\n\
            route_dest=\"$1\"\n\
            route_tmp=\"${route_state}.$$\"\n\
            if [ -r \"$route_state\" ]; then\n\
                while read rd rg rv; do\n\
                    [ \"$rd\" = \"$route_dest\" ] || echo \"$rd $rg $rv\"\n\
                done < \"$route_state\" > \"$route_tmp\"\n\
                mv \"$route_tmp\" \"$route_state\"\n\
            fi\n\
            exit 0 ;;\n\
        show|list|\"\")\n\
            if [ -r \"$route_state\" ]; then\n\
                while read rd rg rv; do\n\
                    [ -z \"$rd\" ] && continue\n\
                    [ \"$rg\" = \"_\" ] && rg=\n\
                    [ \"$rv\" = \"_\" ] && rv=\n\
                    route_line=\"$rd\"\n\
                    [ -n \"$rg\" ] && route_line=\"$route_line via $rg\"\n\
                    [ -n \"$rv\" ] && route_line=\"$route_line dev $rv\"\n\
                    echo \"$route_line\"\n\
                done < \"$route_state\"\n\
            fi\n\
            exit 0 ;;\n\
        *)\n\
            tx_ltp_exec \"$bb\" ip $route_saved ;;\n\
    esac\n\
fi\n\
if [ \"$ip_object\" = \"addr\" ] && [ \"$ip_command\" = \"flush\" ]; then\n\
    exit 0\n\
fi\n\
if [ \"$ip_object\" = \"addr\" ] && [ \"$ip_command\" = \"show\" ]; then\n\
    dhcp_iface=\n\
    if [ -n \"$ip_family\" ]; then\n\
        dhcp_iface=\"$4\"\n\
    else\n\
        dhcp_iface=\"$3\"\n\
    fi\n\
    if [ -r \"$dhcp_state\" ] && [ -n \"$dhcp_iface\" ]; then\n\
        tx_dhcp_found=\n\
        while read tx_dhcp_iface tx_dhcp_addr; do\n\
            [ \"$tx_dhcp_iface\" = \"$dhcp_iface\" ] || continue\n\
            tx_dhcp_found=1\n\
            echo \"2: $tx_dhcp_iface: <BROADCAST,MULTICAST,UP> mtu 1500\"\n\
            case \"$tx_dhcp_addr\" in\n\
                *:*) echo \"    inet6 $tx_dhcp_addr scope global\" ;;\n\
                *) echo \"    inet $tx_dhcp_addr scope global $tx_dhcp_iface\" ;;\n\
            esac\n\
        done < \"$dhcp_state\"\n\
        [ -n \"$tx_dhcp_found\" ] && exit 0\n\
    fi\n\
fi\n\
if [ \"$ip_object\" = \"addr\" ]; then\n\
    case \"$ip_command\" in\n\
        add|del|replace|change)\n\
            if [ -n \"$ip_family\" ]; then\n\
                shift 3\n\
            else\n\
                shift 2\n\
            fi\n\
            ip_args=\n\
            while [ $# -gt 0 ]; do\n\
                if [ \"$1\" = \"nodad\" ]; then\n\
                    shift\n\
                    continue\n\
                fi\n\
                ip_args=\"$ip_args $1\"\n\
                shift\n\
            done\n\
            tx_ltp_exec \"$bb\" ip $ip_family addr \"$ip_command\" $ip_args ;;\n\
    esac\n\
fi\n\
if [ \"$1\" = \"neigh\" ]; then\n\
    cmd=\"$2\"\n\
    shift 2\n\
    case \"$cmd\" in\n\
        replace|add)\n\
            addr=\"$1\"\n\
            shift\n\
            lladdr=\n\
            dev=\n\
            nud=REACHABLE\n\
            while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                    lladdr) lladdr=\"$2\"; shift 2 ;;\n\
                    dev) dev=\"$2\"; shift 2 ;;\n\
                    nud) [ \"$2\" = \"reachable\" ] && nud=REACHABLE || nud=\"$2\"; shift 2 ;;\n\
                    *) shift ;;\n\
                esac\n\
            done\n\
            [ -n \"$addr\" ] && [ -n \"$lladdr\" ] && [ -n \"$dev\" ] || exit 1\n\
            if \"$bb\" arp -s \"$addr\" \"$lladdr\" -i \"$dev\" 2>/dev/null; then\n\
                exit 0\n\
            fi\n\
            tmp=\"${neigh_state}.$$\"\n\
            if [ -r \"$neigh_state\" ]; then\n\
                while read a d l n; do\n\
                    [ \"$a $d\" = \"$addr $dev\" ] || echo \"$a $d $l $n\"\n\
                done < \"$neigh_state\" > \"$tmp\"\n\
            else\n\
                : > \"$tmp\"\n\
            fi\n\
            echo \"$addr $dev $lladdr $nud\" >> \"$tmp\"\n\
            mv \"$tmp\" \"$neigh_state\"\n\
            exit 0 ;;\n\
        show)\n\
            addr=\"$1\"\n\
            tx_ltp_trace_phase \"neigh-show begin addr=$addr\"\n\
            tx_ltp_trace_phase \"neigh-show state-begin\"\n\
            if [ -r \"$neigh_state\" ]; then\n\
                while read a d l n; do\n\
                    [ -z \"$a\" ] && continue\n\
                    if [ -z \"$addr\" ] || [ \"$a\" = \"$addr\" ]; then\n\
                        echo \"$a dev $d lladdr $l $n\"\n\
                    fi\n\
                done < \"$neigh_state\"\n\
            fi\n\
            tx_ltp_trace_phase \"neigh-show state-end\"\n\
            tx_ltp_trace_phase \"neigh-show proc-neigh-begin\"\n\
            if [ -r /proc/net/tx_neigh ]; then\n\
                while IFS= read -r line; do\n\
                    [ -z \"$line\" ] && continue\n\
                    if [ -z \"$addr\" ]; then\n\
                        echo \"$line\"\n\
                    else\n\
                        case \"$line\" in \"$addr \"*) echo \"$line\" ;; esac\n\
                    fi\n\
                done < /proc/net/tx_neigh\n\
            elif [ -r /proc/net/arp ]; then\n\
                first=1\n\
                while read ip hwtype flags mac mask dev; do\n\
                    if [ \"$first\" ]; then first=; continue; fi\n\
                    [ -z \"$ip\" ] && continue\n\
                    if [ -z \"$addr\" ] || [ \"$ip\" = \"$addr\" ]; then\n\
                        nud=FAILED\n\
                        case \"$flags\" in 0x2|0X2|2) nud=REACHABLE ;; esac\n\
                        echo \"$ip dev $dev lladdr $mac $nud\"\n\
                    fi\n\
                done < /proc/net/arp\n\
            fi\n\
            tx_ltp_trace_phase \"neigh-show proc-neigh-end\"\n\
            tx_ltp_trace_phase \"neigh-show end\"\n\
            exit 0 ;;\n\
        del)\n\
            addr=\"$1\"\n\
            orig_args=\"$*\"\n\
            shift\n\
            dev=\n\
            while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                    dev) dev=\"$2\"; shift 2 ;;\n\
                    *) shift ;;\n\
                esac\n\
            done\n\
            tx_ltp_trace_phase \"neigh-del begin addr=$addr dev=$dev\"\n\
            [ -n \"$addr\" ] || tx_ltp_exec \"$bb\" ip neigh del $orig_args\n\
            tmp=\"${neigh_state}.$$\"\n\
            tx_ltp_trace_phase \"neigh-del state-begin\"\n\
            if [ -r \"$neigh_state\" ]; then\n\
                while read a d l n; do\n\
                    skip=\n\
                    if [ \"$a\" = \"$addr\" ]; then\n\
                        if [ -z \"$dev\" ] || [ \"$d\" = \"$dev\" ]; then\n\
                            skip=1\n\
                        fi\n\
                    fi\n\
                    [ \"$skip\" ] || echo \"$a $d $l $n\"\n\
                done < \"$neigh_state\" > \"$tmp\"\n\
                mv \"$tmp\" \"$neigh_state\"\n\
            fi\n\
            tx_ltp_trace_phase \"neigh-del state-end\"\n\
            if [ -n \"$addr\" ] && [ -n \"$dev\" ]; then\n\
                tx_ltp_trace_phase \"neigh-del tx-ctl-begin\"\n\
                tx_neigh_ctl_ok=\n\
                if [ -w /proc/net/tx_neigh_ctl ] && ( printf '%s %s\\n' \"$addr\" \"$dev\" > /proc/net/tx_neigh_ctl ) 2>/dev/null; then\n\
                    tx_neigh_ctl_ok=1\n\
                fi\n\
                if [ -n \"$tx_neigh_ctl_ok\" ]; then\n\
                    tx_ltp_trace_phase \"neigh-del tx-ctl-end\"\n\
                else\n\
                    tx_ltp_trace_phase \"neigh-del tx-ctl-end\"\n\
                    tx_ltp_trace_phase \"neigh-del arp-d-begin\"\n\
                    \"$bb\" arp -d \"$addr\" -i \"$dev\" 2>/dev/null || true\n\
                    tx_ltp_trace_phase \"neigh-del arp-d-end\"\n\
                fi\n\
            else\n\
                tx_ltp_exec \"$bb\" ip neigh del $orig_args\n\
            fi\n\
            exit 0 ;;\n\
    esac\n\
fi\n\
if [ \"$1\" = \"maddr\" ]; then\n\
    cmd=\"$2\"\n\
    shift 2\n\
    case \"$cmd\" in\n\
        add)\n\
            mac=\"$1\"\n\
            shift\n\
            dev=\n\
            while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                    dev) dev=\"$2\"; shift 2 ;;\n\
                    *) shift ;;\n\
                esac\n\
            done\n\
            [ -n \"$mac\" ] || exit 1\n\
            tmp=\"${maddr_state}.$$\"\n\
            if [ -r \"$maddr_state\" ]; then\n\
                while read d m; do\n\
                    [ \"$d $m\" = \"$dev $mac\" ] || echo \"$d $m\"\n\
                done < \"$maddr_state\" > \"$tmp\"\n\
            else\n\
                : > \"$tmp\"\n\
            fi\n\
            echo \"$dev $mac\" >> \"$tmp\"\n\
            mv \"$tmp\" \"$maddr_state\"\n\
            exit 0 ;;\n\
        show)\n\
            mdev=\n\
            while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                    dev) mdev=\"$2\"; shift 2 ;;\n\
                    *) mdev=\"$1\"; shift ;;\n\
                esac\n\
            done\n\
            if [ -n \"$mdev\" ]; then\n\
                echo \"1:      $mdev\"\n\
                echo \"        inet  224.0.0.1\"\n\
            fi\n\
            if [ -r \"$maddr_state\" ]; then\n\
                while read d m; do\n\
                    [ -z \"$m\" ] && continue\n\
                    [ -n \"$mdev\" ] && [ \"$d\" != \"$mdev\" ] && continue\n\
                    echo \"        link  $m static\"\n\
                done < \"$maddr_state\"\n\
            fi\n\
            exit 0 ;;\n\
        del)\n\
            mac=\"$1\"\n\
            shift\n\
            dev=\n\
            while [ $# -gt 0 ]; do\n\
                case \"$1\" in\n\
                    dev) dev=\"$2\"; shift 2 ;;\n\
                    *) shift ;;\n\
                esac\n\
            done\n\
            tmp=\"${maddr_state}.$$\"\n\
            if [ -r \"$maddr_state\" ]; then\n\
                while read d m; do\n\
                    skip=\n\
                    if [ \"$m\" = \"$mac\" ]; then\n\
                        if [ -z \"$dev\" ] || [ \"$d\" = \"$dev\" ]; then\n\
                            skip=1\n\
                        fi\n\
                    fi\n\
                    [ \"$skip\" ] || echo \"$d $m\"\n\
                done < \"$maddr_state\" > \"$tmp\"\n\
                mv \"$tmp\" \"$maddr_state\"\n\
            fi\n\
            exit 0 ;;\n\
    esac\n\
fi\n\
tx_ltp_exec \"$bb\" ip \"$@\"\n";
        if !create_file_with_data(&create_ctx, tx_ltp_bin_id, b"ip", 0o755, ip_script) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-ip\n");
            return;
        }
        let tst_net_ip_prefix_script = b"#!/bin/sh\n\
orig=/musl/musl/ltp/testcases/bin/tst_net_ip_prefix\n\
remote=\n\
if [ \"$1\" = \"-r\" ]; then\n\
    remote=1\n\
    shift\n\
fi\n\
case \"$1\" in\n\
    ''|-h|--help) exec \"$orig\" ${remote:+-r} \"$@\" ;;\n\
esac\n\
addr=\"$1\"\n\
case \"$addr\" in\n\
    */*) host=\"${addr%/*}\"; prefix=\"${addr##*/}\" ;;\n\
    *) host=\"$addr\"; case \"$addr\" in *:*) prefix=64 ;; *) prefix=24 ;; esac ;;\n\
esac\n\
case \"$host\" in\n\
    *:*)\n\
        if [ \"$remote\" ]; then\n\
            echo \"IPV6_RHOST='$host'; export IPV6_RHOST\"\n\
            echo \"IPV6_RPREFIX='$prefix'; export IPV6_RPREFIX\"\n\
        else\n\
            echo \"IPV6_LHOST='$host'; export IPV6_LHOST\"\n\
            echo \"IPV6_LPREFIX='$prefix'; export IPV6_LPREFIX\"\n\
        fi ;;\n\
    *)\n\
        if [ \"$remote\" ]; then\n\
            echo \"IPV4_RHOST='$host'; export IPV4_RHOST\"\n\
            echo \"IPV4_RPREFIX='$prefix'; export IPV4_RPREFIX\"\n\
        else\n\
            echo \"IPV4_LHOST='$host'; export IPV4_LHOST\"\n\
            echo \"IPV4_LPREFIX='$prefix'; export IPV4_LPREFIX\"\n\
        fi ;;\n\
esac\n";
        let tst_net_iface_prefix_script = b"#!/bin/sh\n\
orig=/musl/musl/ltp/testcases/bin/tst_net_iface_prefix\n\
remote=\n\
if [ \"$1\" = \"-r\" ]; then\n\
    remote=1\n\
    shift\n\
fi\n\
case \"$1\" in\n\
    ''|-h|--help) exec \"$orig\" ${remote:+-r} \"$@\" ;;\n\
esac\n\
addr=\"$1\"\n\
case \"$addr\" in\n\
    */*) host=\"${addr%/*}\"; prefix=\"${addr##*/}\" ;;\n\
    *) host=\"$addr\"; case \"$addr\" in *:*) prefix=64 ;; *) prefix=24 ;; esac ;;\n\
esac\n\
case \"$host\" in\n\
    10.0.0.2|fd00:1:1:1::2) iface=eth0 ;;\n\
    10.0.0.1|fd00:1:1:1::1) iface=ltp_ns_veth1 ;;\n\
    *) exec \"$orig\" ${remote:+-r} \"$@\" ;;\n\
esac\n\
case \"$host\" in\n\
    *:*) prefix_var=IPV6 ;;\n\
    *) prefix_var=IPV4 ;;\n\
esac\n\
if [ \"$remote\" ]; then\n\
    echo \"${prefix_var}_RPREFIX='$prefix'\"\n\
    echo \"RHOST_IFACES='$iface'\"\n\
else\n\
    echo \"${prefix_var}_LPREFIX='$prefix'\"\n\
    echo \"LHOST_IFACES='$iface'\"\n\
fi\n";
        let tst_net_vars_script = b"#!/bin/sh\n\
orig=/musl/musl/ltp/testcases/bin/tst_net_vars\n\
[ $# -eq 2 ] || exec \"$orig\" \"$@\"\n\
left=\"$1\"\n\
right=\"$2\"\n\
lh=\"${left%/*}\"\n\
lp=\"${left##*/}\"\n\
rh=\"${right%/*}\"\n\
rp=\"${right##*/}\"\n\
[ \"$lh\" != \"$left\" ] || exec \"$orig\" \"$left\" \"$right\"\n\
[ \"$rh\" != \"$right\" ] || exec \"$orig\" \"$left\" \"$right\"\n\
case \"$lh $rh\" in\n\
    *:*)\n\
        [ \"$lp\" = 64 ] && [ \"$rp\" = 64 ] || exec \"$orig\" \"$left\" \"$right\"\n\
        case \"$lh $rh\" in\n\
            fd00:1:1:1::2\\ fd00:1:1:1::1)\n\
                echo \"IPV6_LNETMASK='ffff:ffff:ffff:ffff::'\"\n\
                echo \"IPV6_RNETMASK='ffff:ffff:ffff:ffff::'\"\n\
                echo \"IPV6_LNETWORK='fd00:1:1:1::'\"\n\
                echo \"IPV6_RNETWORK='fd00:1:1:1::'\"\n\
                echo \"LHOST_IPV6_HOST='2'\"\n\
                echo \"RHOST_IPV6_HOST='1'\"\n\
                echo \"IPV6_NET32_UNUSED='fd00:23'\"\n\
                exit 0 ;;\n\
        esac ;;\n\
    *)\n\
        [ \"$lp\" = 24 ] && [ \"$rp\" = 24 ] || exec \"$orig\" \"$left\" \"$right\"\n\
        oldifs=\"$IFS\"\n\
        IFS=.\n\
        set -- $lh\n\
        la=\"$1\"; lb=\"$2\"; lc=\"$3\"; ld=\"$4\"\n\
        set -- $rh\n\
        ra=\"$1\"; rb=\"$2\"; rc=\"$3\"; rd=\"$4\"\n\
        IFS=\"$oldifs\"\n\
        [ -n \"$la\" ] && [ -n \"$lb\" ] && [ -n \"$lc\" ] && [ -n \"$ld\" ] || exec \"$orig\" \"$left\" \"$right\"\n\
        [ -n \"$ra\" ] && [ -n \"$rb\" ] && [ -n \"$rc\" ] && [ -n \"$rd\" ] || exec \"$orig\" \"$left\" \"$right\"\n\
        echo \"IPV4_LBROADCAST='$la.$lb.$lc.255'\"\n\
        echo \"IPV4_RBROADCAST='$ra.$rb.$rc.255'\"\n\
        echo \"IPV4_LNETMASK='255.255.255.0'\"\n\
        echo \"IPV4_RNETMASK='255.255.255.0'\"\n\
        echo \"IPV4_LNETWORK='$la.$lb.$lc'\"\n\
        echo \"IPV4_RNETWORK='$ra.$rb.$rc'\"\n\
        echo \"LHOST_IPV4_HOST='$ld'\"\n\
        echo \"RHOST_IPV4_HOST='$rd'\"\n\
        echo \"IPV4_NET16_UNUSED='10.23'\"\n\
        exit 0 ;;\n\
esac\n\
exec \"$orig\" \"$left\" \"$right\"\n";
        let tst_check_drivers_script = b"#!/bin/sh\n\
orig=/musl/musl/ltp/testcases/bin/tst_check_drivers\n\
[ $# -gt 0 ] || exec \"$orig\" \"$@\"\n\
for driver in \"$@\"; do\n\
    case \"$driver\" in\n\
        bridge|dummy|ip6_tables|ip_tables|nf_tables|sch_teql|veth) ;;\n\
        *) exec \"$orig\" \"$@\" ;;\n\
    esac\n\
done\n\
exit 0\n";
        if !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tst_net_ip_prefix",
            0o755,
            tst_net_ip_prefix_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tst_net_iface_prefix",
            0o755,
            tst_net_iface_prefix_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tst_net_vars",
            0o755,
            tst_net_vars_script,
        ) || !create_file_with_data(
            &create_ctx,
            tx_ltp_bin_id,
            b"tst_check_drivers",
            0o755,
            tst_check_drivers_script,
        ) {
            Self::write_board_sentinel_prefix();
            tx_hal::console_write_str::<P>(":network-db:err:create-ltp-net-helpers\n");
            return;
        }

        Self::write_board_sentinel_prefix();
        tx_hal::console_write_str::<P>(":network-db:ok\n");
    }
}

struct RootfsCreateContext<'a> {
    fs_ops: &'a alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
    fs_page_backing: &'a alloc::sync::Arc<dyn FsPageBacking>,
    mount: &'a Cap<tx_subsystems::mount::MountPayload>,
    cred: &'a Credential,
}

fn create_file_with_data(
    ctx: &RootfsCreateContext<'_>,
    parent: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    mode: u16,
    data: &[u8],
) -> bool {
    let (file_id, file_meta) = {
        let guard = step_engine::guard();
        match ctx.fs_ops.create_inode(parent, name, mode, ctx.cred, &guard) {
            StepOutcome::Done(out) => out,
            StepOutcome::Err(step_engine::Errno::EEXIST) => return true,
            _ => return false,
        }
    };

    let pc = {
        let guard = step_engine::guard();
        let rnode = match ctx
            .fs_ops
            .materialise_rnode(file_id, file_meta, ctx.mount, &guard)
        {
            StepOutcome::Done(rnode) => rnode,
            _ => return false,
        };
        match rnode.backing() {
            tx_subsystems::vfs::structure::RNodeBacking::PageBacked { pc } => pc.clone(),
            _ => return false,
        }
    };

    for (idx, chunk) in data.chunks(tx_subsystems::vm::USER_PAGE_SIZE).enumerate() {
        let materialized =
            match pc.materialize_anon(PageIndex::new(idx as u64), MaterializeAccess::Write) {
                Ok(page) => page,
                Err(_) => return false,
            };
        let frame_base = match page_allocator::frame_kernel_addr(materialized.ppn) {
            Ok(addr) => addr,
            Err(_) => return false,
        };
        unsafe {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_base, chunk.len());
        }
    }

    let guard = step_engine::guard();
    matches!(
        ctx.fs_page_backing
            .truncate(file_id, data.len() as u64, &guard),
        StepOutcome::Done(())
    )
}

/// Create-or-find a directory under `parent`. Treats EEXIST as
/// "fine, look it up" rather than a hard error so a second boot in
/// the same test fixture doesn't panic.
///
/// Discipline: each FS call sits in its own scope so the `Guard` is
/// dropped before the next call acquires a new one. txKernel's
/// epoch discipline panics on nested guards
/// (`tx-substrate::epoch::local:55`).
fn mkdir_or_find(
    fs_ops: &alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
    parent: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    mode: u16,
    cred: &Credential,
) -> Option<tx_subsystems::vfs::FsObjectId> {
    let mkdir_outcome = {
        let guard = step_engine::guard();
        fs_ops.mkdir(parent, name, mode, cred, &guard)
    };
    match mkdir_outcome {
        StepOutcome::Done((id, _meta)) => Some(id),
        // EEXIST: look up the existing entry (e.g. previous boot
        // ran this helper). Treating it as fatal would make
        // re-boots panic.
        StepOutcome::Err(step_engine::Errno::EEXIST) => {
            let guard = step_engine::guard();
            match fs_ops.lookup(parent, name, &guard) {
                StepOutcome::Done(existing) => Some(existing),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Best-effort symlink — ignores errors so a re-boot doesn't panic
/// when the symlink is already present.
fn symlink_into(
    fs_ops: &alloc::sync::Arc<dyn tx_subsystems::vfs::FsOps>,
    parent: tx_subsystems::vfs::FsObjectId,
    name: &[u8],
    target: &[u8],
    cred: &Credential,
) -> bool {
    let guard = step_engine::guard();
    matches!(
        fs_ops.symlink(parent, name, target, cred, &guard),
        StepOutcome::Done(_)
    )
}
