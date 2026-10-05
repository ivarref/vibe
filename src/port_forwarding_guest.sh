#!/bin/bash
# Report the TCP ports the guest listens on to the host, as "+<port>" / "-<port>" lines, and relay
# vsock port <port> to each of them, so the host can forward localhost:<port> into the guest.
#
# ss is the source of truth for what's listening. bpftrace only tells us when to look again:
# it prints a line whenever a TCP socket starts or stops listening. Without bpftrace (or if it
# dies), fall back to polling every second.
(
    exec 3> /dev/virtio-ports/vibe-ports

    # The host serves Docker's socket by connecting to this vsock port (DOCKER_VSOCK_PORT in
    # port_forwarding.rs). Connections fail until dockerd is up, which is fine.
    socat VSOCK-LISTEN:100000,fork UNIX-CONNECT:/var/run/docker.sock &
    declare -A relays current

    sync_ports() {
        current=()
        # "<port> <address>" for each listening socket, e.g. "5173 127.0.0.1" or "5173 [::1]".
        while read -r port addr; do
            # Prefer IPv4 when a port is listened on with both.
            if [ -z "${current[$port]-}" ] || [[ "$addr" != \[* ]]; then
                current[$port]=$addr
            fi
        # Skip systemd-resolved (DNS and LLMNR), which is of no use on the host.
        done < <(ss -Hltnp | awk '/"systemd-resolve"/ { next } { a = $4; p = a; sub(/.*:/, "", p); sub(/:[0-9]+$/, "", a); sub(/%[^\]]*/, "", a); print p, a }')

        for port in "${!current[@]}"; do
            [ -n "${relays[$port]-}" ] && continue
            case "${current[$port]}" in
                0.0.0.0 | \*) target=127.0.0.1 ;;
                \[::\]) target='[::1]' ;;
                *) target=${current[$port]} ;;
            esac
            socat "VSOCK-LISTEN:$port,fork,reuseaddr" "TCP:$target:$port" &
            relays[$port]=$!
            echo "+$port" >&3
        done

        for port in "${!relays[@]}"; do
            if [ -z "${current[$port]-}" ]; then
                kill "${relays[$port]}"
                unset "relays[$port]"
                echo "-$port" >&3
            fi
        done
    }

    # Prints one line per listen/unlisten, plus "Attaching 1 probe..." once the probe is live, which
    # triggers a sync that catches anything that started listening while bpftrace was starting up.
    # A connection to a listener also leaves TCP_LISTEN (the new socket is copied from the
    # listener and moves to TCP_SYN_RECV), so only count LISTEN -> CLOSE as unlistening.
    if command -v bpftrace > /dev/null; then
        exec 4< <(bpftrace -B line -e '
            #define IPPROTO_TCP 6
            #define TCP_CLOSE   7
            #define TCP_LISTEN  10

            tracepoint:sock:inet_sock_set_state
            /args.protocol == IPPROTO_TCP &&
             (args.newstate == TCP_LISTEN || (args.oldstate == TCP_LISTEN && args.newstate == TCP_CLOSE))/
            {
                printf("%d\n", args.sport);
            }')
    else
        exec 4< /dev/null
    fi

    while true; do
        sync_ports
        # Wait for an event; every 10s sync anyway, as a safety net.
        read -r -t 10 _ <&4
        status=$?
        # read returns > 128 on timeout, and 1 at end of file (no bpftrace, or it exited).
        if [ "$status" -ne 0 ] && [ "$status" -le 128 ]; then
            sleep 1
        fi
        # Fold a burst of events (e.g. a server listening on IPv4 and IPv6) into one sync.
        while read -r -t 0.05 _ <&4; do :; done
    done
) > /dev/null 2>&1 &
