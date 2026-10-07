# Proxmox deployment notes

These notes are generic and apply to either a native Debian-based Proxmox
host or a dedicated unprivileged LXC container. They intentionally contain no
host-specific IP addresses, VM IDs, bridge names, or hardware assumptions.

## Native host profile

Use the systemd unit in contrib/systemd with a dedicated non-root account.
This profile observes the host's actual route and can access local optional
devices without passthrough. Keep the binary, state directory, incident
directory, and unit independently backed up.

## LXC profile

Use a dedicated unprivileged container with a deliberately chosen network
attachment. Verify the route seen from inside the container before relying on
path classification. NAT, an internal bridge, or SDN can add a hop that is
not present on the physical LAN.

If the optional speaker capability is required inside the container, pass
through only the specific evdev device and grant the service account narrowly
scoped write access. Do not make the container privileged solely for sound.

The application remains useful without speaker passthrough; incidents and
traceroute evidence do not depend on audio.

## Acceptance checks

~~~sh
ip route
ip route get <trace-target>
/usr/local/bin/ping_monitor --check-config
systemctl is-active ping-monitor.service
~~~

Run a controlled outage test after deployment. Confirm one incident start,
one incident end, one automatic trace, and a clean service shutdown.
