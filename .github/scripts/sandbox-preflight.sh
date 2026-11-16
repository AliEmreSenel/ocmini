#!/bin/sh
set -eu

# Exercise the required host features before spending time on a Rust build.
# The outer container is privileged; ocmini and its tests still run unprivileged.
. /etc/os-release
test "$ID" = debian
test "$(id -u)" -ne 0
uname -a
id
command -v rg
test -r /dev/net/tun
test -w /dev/net/tun

ocmini_probe_dir=$(mktemp -d)
trap 'rm -rf "$ocmini_probe_dir"' EXIT HUP INT TERM
cat > "$ocmini_probe_dir/probe.c" <<'EOF'
#include <fcntl.h>
#include <linux/if.h>
#include <linux/if_tun.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    long abi = syscall(SYS_landlock_create_ruleset, NULL, 0, 1);
    printf("Landlock ABI: %ld (required: 6 or newer)\n", abi);
    if (abi < 6) return 1;
    int tun = open("/dev/net/tun", O_RDWR | O_CLOEXEC);
    if (tun < 0) {
        perror("open /dev/net/tun");
        return 1;
    }
    struct ifreq interface = { .ifr_flags = IFF_TUN | IFF_NO_PI };
    if (ioctl(tun, TUNSETIFF, &interface) < 0) {
        perror("create nested TUN interface");
        close(tun);
        return 1;
    }
    close(tun);
    puts("Nested namespaces and TUN interface: available");
    return 0;
}
EOF
cc "$ocmini_probe_dir/probe.c" -o "$ocmini_probe_dir/probe"
unshare --user --map-root-user --mount --net --pid --fork --mount-proc "$ocmini_probe_dir/probe"
