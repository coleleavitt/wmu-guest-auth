# Reference: WMU Guest Portal Artifacts

Beautified HTML/JS/CSS captured from the WMU Guest WiFi captive portal flow.
Regenerate with:

```
wmu-guest-auth dump --output ./wmu-dump
js-beautify      -o reference/js/loginscript.js   wmu-dump/js/loginscript.js
js-beautify --css -o reference/css/patterns.css    wmu-dump/css/patterns.css
js-beautify --html -o reference/html/portal.html   wmu-dump/html/portal.html
js-beautify --html -o reference/html/wlc-login.html wmu-dump/html/wlc-login.html
```

## Flow summary

1. Client connects to `WMU Guest` → DHCP lease → DNS intercepted by Cisco WLC.
2. Probe to `http://connectivitycheck.gstatic.com/generate_204` returns
   `HTTP 200` with a `Location` header **and** a
   `<meta http-equiv="refresh" url="...">` body pointing at:

   ```
   https://legacy.wmich.edu/oit/guest/wmu-guest-policy.html
     ?switch_url=https://virtual.wireless.wmich.edu/login.html
     &ap_mac=<AP MAC>
     &client_mac=<CLIENT MAC>
     &wlan=WMU%20Guest
     &redirect=<ORIGINAL HOST>/<ORIGINAL PATH>
   ```

3. `legacy.wmich.edu/.../wmu-guest-policy.html` (`html/portal.html`) is the
   branded Accept-policy page. Its form has **no `action=` attribute**; the
   action is set dynamically by `loadAction()` to the value of the
   `switch_url` query parameter.
4. Clicking **Accept** runs `submitAction()`:
   - Reads `redirect=` from the current URL
   - Builds `redirect_url = "http://www.wmich.edu" + <rest>` (note: Cisco/WMU
     bug — always produces a malformed URL, but the WLC ignores the value)
   - Sets `buttonClicked = 4`
   - POSTs the form to `switch_url` (i.e. `virtual.wireless.wmich.edu/login.html`)
5. Cisco WLC accepts the POST, transitions the client MAC from
   `WEBAUTH_REQD` → `RUN` state, and the client is released.

## Form schemas

### `portal.html` (policy page)
| Field | Type | Default | Purpose |
|---|---|---|---|
| `buttonClicked` | hidden | `0` | Set to `4` by `submitAction()` = "Accept" |
| `redirect_url`  | hidden | `""`  | Where WLC redirects after auth success |
| `err_flag`      | hidden | `0`   | `1` if prior auth attempt failed |

### `wlc-login.html` (Cisco WLC's own login page at `/login.html`)
The Cisco-native consent/webauth page. Not shown in normal WMU flow (policy
page posts directly to `/login.html`), but this is what the endpoint
accepts:

| Field | Type | Default | Required? | Notes |
|---|---|---|---|---|
| `buttonClicked` | hidden | `0` | **yes** | Must be `4` for Accept |
| `err_flag`      | hidden | `0` | **yes** | `0` on first attempt |
| `err_msg`       | hidden | `""` | no | |
| `info_flag`     | hidden | `0` | no | |
| `info_msg`      | hidden | `""` | no | |
| `redirect_url`  | hidden | `""` | no | Not validated |
| `network_name`  | hidden | `"Guest Network"` | no | Display-only |

`loginscript.js::submitAction()` confirms `buttonClicked = 4` is the
accept-click value and shows an `err_flag == 1` retry branch.

## Cisco WLC quirks observed

1. **`HTTP 200` with `Location` header** (non-standard) on the
   `/generate_204` probe. We treat any `Location` header as captive
   regardless of status code.
2. **Meta-refresh URL differs from `Location` URL** — the body's meta tag
   contains a subset of the redirect parameters. Both work for reaching
   the policy page; pick whichever parses first.
3. **GET `login.html` before POST** establishes WLC session state. Skipping
   the GET causes the POST to return HTTP 200 but the client MAC is never
   promoted to the authed list.
4. **Self-signed TLS cert** on `virtual.wireless.wmich.edu`. Standard for
   Cisco WLC virtual interfaces.
