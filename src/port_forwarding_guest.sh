#!/bin/bash
# Report the TCP ports the guest listens on to the host, as "+<port>" / "-<port>" lines, and relay
# vsock port <port> to each of them, so the host can forward localhost:<port> into the guest.
(
    exec 3> /dev/virtio-ports/vibe-ports
    declare -A relays current
    while true; do
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

        sleep 1
    done
) > /dev/null 2>&1 &
