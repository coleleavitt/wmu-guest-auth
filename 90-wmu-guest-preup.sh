#!/bin/bash
INTERFACE="$1"
ACTION="$2"
CONNECTION_ID="$CONNECTION_ID"

[ "$CONNECTION_ID" = "WMU Guest" ] || exit 0
[ "$ACTION" = "pre-up" ] || exit 0

logger -t wmu-guest-auth "pre-up triggered: interface=$INTERFACE"
/usr/local/bin/wmu-guest-auth auto-auth \
    --interface "$INTERFACE" \
    --dhcp-timeout 10 \
    --retries 3 \
    --delay 2 \
    2>&1 | logger -t wmu-guest-auth
logger -t wmu-guest-auth "pre-up done (exit=$?)"
exit 0
