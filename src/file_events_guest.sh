#!/bin/bash
# Expects WATCHED (array of guest paths) and EXCLUDE (regex of masked paths) to be defined above.
#
# The host writes "<mtime> <path>" lines for files it saw change ("-" as mtime if deleted, followed
# by a line for the parent directory).
# Virtiofs doesn't tell the guest kernel about host-side changes, so inotify watchers never hear about them.
# Setting the mtime to the value it already has triggers an inotify IN_ATTRIB event without modifying anything.
#
# The host also sees (and forwards) the guest's own writes. To avoid a duplicate event for those,
# record the mtime of every path the guest changes itself (and of its parent directory, for
# creations and removals) and skip forwarded paths that still have it.
(
    {
        if command -v inotifywait > /dev/null; then
            stdbuf -oL inotifywait -m -r -q \
                -e close_write -e create -e moved_to -e moved_from -e delete \
                --exclude "$EXCLUDE" --format 'G %e %w%f' "${WATCHED[@]}" &
        fi
        sed -u 's/^/H /' < /dev/virtio-ports/vibe-file-events
    } | {
        declare -A own
        while IFS=' ' read -r source a p; do
            if [ "$source" = G ]; then
                case "$a" in
                    *DELETE* | *MOVED_FROM*) own[$p]=- ;;
                    *) own[$p]=$(stat -c %.9Y "$p" 2> /dev/null) ;;
                esac
                if [ "$a" != CLOSE_WRITE,CLOSE ]; then
                    d="${p%/*}"
                    own[$d]=$(stat -c %.9Y "$d" 2> /dev/null)
                fi
                [ "${#own[@]}" -gt 100000 ] && own=()
            elif [ "$a" = - ] || [ "${own[$p]-}" = "$a" ]; then
                continue
            else
                touch -c -h -m -d "@$a" "$p"
            fi
        done
    } > /dev/null 2>&1 &
)
