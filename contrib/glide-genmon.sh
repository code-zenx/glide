#!/bin/sh
# The glide badge for Xfce's top bar, through the Generic Monitor (genmon)
# panel plugin: coloured by state, with the status as its tooltip.
#
# Install (X11 client, e.g. nullbyte):
#   install -m755 contrib/glide-genmon.sh ~/.local/bin/glide-genmon
#   mkdir -p ~/.local/share/glide/badges
#   for f in contrib/badges/*.svg; do
#     sed 's/width="64" height="64"/width="20" height="20"/' "$f" > ~/.local/share/glide/badges/"${f##*/}"
#   done
# then add a Generic Monitor to the panel with this as its command, label
# off, period 1 s.
#
# Badge: purple linked, gold sending, cyan receiving, orange paused,
# grey offline, faded grey switched off by the server, red needs permission.

badges="$HOME/.local/share/glide/badges"
glide="$HOME/.local/bin/glide"

# The status file outlives the daemon, so ask systemd whether it is running.
if systemctl --user is-active --quiet glide.service; then
    json=$("$glide" status --json 2>/dev/null)
    class=$(printf '%s' "$json" | sed -n 's/.*"class":"\([^"]*\)".*/\1/p')
    tip=$(printf '%s' "$json" | sed -n 's/.*"tooltip":"\([^"]*\)".*/\1/p')
else
    class=offline
    tip="glide is not running"
fi
[ -f "$badges/glide-$class.svg" ] || class=offline

echo "<img>$badges/glide-$class.svg</img>"
echo "<tool>${tip:-glide}</tool>"
