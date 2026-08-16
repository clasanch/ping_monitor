# systemd deployment

This directory contains a generic Linux service definition. It does not
assume a particular board, interface name, IP address, distribution, or
virtualization layout.

## Install

Build a release binary with Rust 1.85 or newer and copy it to:

~~~text
/usr/local/bin/ping_monitor
~~~

Create a dedicated account and directories:

~~~sh
useradd --system --home-dir /var/lib/ping-monitor --shell /usr/sbin/nologin ping-monitor
install -d -o ping-monitor -g ping-monitor -m 0750 /etc/ping-monitor
install -d -o ping-monitor -g ping-monitor -m 0750 /var/lib/ping-monitor
install -d -o ping-monitor -g ping-monitor -m 0750 /var/log/ping-monitor
install -o root -g root -m 0644 contrib/systemd/ping-monitor.env.example /etc/ping-monitor/ping-monitor.env
install -o root -g root -m 0644 contrib/systemd/ping-monitor.service /etc/systemd/system/ping-monitor.service
~~~

Replace the example targets with targets suitable for the local network.
Validate before starting:

~~~sh
/usr/local/bin/ping_monitor --check-config
systemd-analyze verify /etc/systemd/system/ping-monitor.service
systemctl daemon-reload
systemctl enable --now ping-monitor.service
systemctl status ping-monitor.service
~~~

Automatic path diagnosis uses the first available native utility. On Linux,
provide `tracepath` or `traceroute` using the package names appropriate for the
distribution (for example, `iputils-tracepath` or `traceroute` on Debian-based
systems). Windows provides `tracert` as part of the operating system. If no
utility is available, the monitor remains healthy and writes an `unavailable`
TRACE record with the error as evidence.

Logs:

~~~sh
journalctl -u ping-monitor.service -f
find /var/log/ping-monitor -maxdepth 2 -type f -print
~~~

The service uses Wants=network-online.target for ordering only. A network
failure is a condition to monitor, not a reason to make the service depend
hard on another network service.

## PC speaker permissions

The speaker backend is optional and disabled by default. On Linux, grant the
dedicated service account access only to the selected evdev device using a
distribution-specific udev rule and a dedicated group. Do not add the account
to the broad input group, use setuid, or run the monitor as root.

After changing the rule, verify the device path and permissions, then set:

~~~text
PM_SOUND_BACKEND=pc-speaker
PM_PC_SPEAKER_DEVICE=/dev/input/by-path/platform-pcspkr-event-spkr
~~~

The path above is a conventional Linux capability path, not a board-specific
requirement. If the device is absent or unwritable, monitoring and incident
logging continue with sound disabled.
