#!/bin/bash
INTERFACE="$1"
ACTION="$2"
CONNECTION_ID="$CONNECTION_ID"

[ "$CONNECTION_ID" = "WMU Guest" ] || exit 0
[ "$ACTION" = "up" ] || [ "$ACTION" = "connectivity-change" ] || exit 0

logger -t wmu-guest-auth "triggered: interface=$INTERFACE action=$ACTION"
/usr/local/bin/wmu-guest-auth auto-auth --retries 5 --delay 3 2>&1 | logger -t wmu-guest-auth &
