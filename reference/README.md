# Reference: WMU Guest Portal Artifacts

Captured HTML/JS/CSS from the Cisco WLC + WMU captive portal flow.

Both raw (as-downloaded) and beautified/deobfuscated versions are committed
for side-by-side comparison.

## Layout

```
reference/
├── html/
│   ├── portal.raw.html              WMU policy page (legacy.wmich.edu)
│   ├── portal.beautified.html       ↑ js-beautify
│   ├── wlc-login.raw.html           Cisco WLC native (virtual.wireless.wmich.edu/login.html)
│   └── wlc-login.beautified.html    ↑ js-beautify
├── js/
│   ├── loginscript.js               Cisco WLC /loginscript.js (referenced by wlc-login.html)
│   ├── inline-portal-0.js           WMU's submitAction + loadAction (from portal.html <script>)
│   ├── inline-wlc-login-1.js        Cisco's getErrorMsgIfAny + unhideform
│   ├── inline-wlc-login-2.js        Empty <script> tag (placeholder)
│   └── inline-wlc-login-3.js        Cisco's getHtmlForButton call
├── js-deobf/
│   └── *.js                         Same files run through jsbeautify -d (oxc, 19-phase pipeline)
├── css/
│   ├── patterns.raw.css             WMU Patterns design system (minified 128KB)
│   └── patterns.beautified.css      ↑ beautified to 171KB / 9155 lines
└── fonts/
    └── montserrat.css               Google Fonts CSS referenced by patterns.css
```

## Regeneration

```
wmu-guest-auth dump --output ./wmu-dump
jsbeautify -d -o reference/js-deobf/loginscript.js wmu-dump/js/loginscript.js
# (see scripts/re-dump.sh for full regeneration)
```

## The Auth Flow

1. Client joins `WMU Guest` → DHCP → DNS hijacked by WLC.
2. Probe `http://connectivitycheck.gstatic.com/generate_204` returns
   `HTTP 200` with `Location` header + `<meta http-equiv=refresh>` body
   pointing to:
   ```
   https://legacy.wmich.edu/oit/guest/wmu-guest-policy.html
     ?switch_url=https://virtual.wireless.wmich.edu/login.html
     &ap_mac=<AP MAC>
     &client_mac=<CLIENT MAC>        ← may be absent on Variant A
     &wlan=WMU%20Guest
     &redirect=<ORIGINAL HOST>       ← Variant B only
     &statusCode=<INT>               ← Variant A only (1=already logged in)
   ```
3. `portal.html` loads; `loadAction()` sets `<form action>` to `switch_url`.
4. User clicks Accept → `submitAction()` builds `redirect_url`, sets
   `buttonClicked=4`, POSTs form to `switch_url`.
5. WLC promotes client MAC from `WEBAUTH_REQD` → `RUN`. Client is online.

## URL Variants

Two shapes observed in the wild (across multiple APs):

### Variant A (no client_mac, has statusCode)
```
?switch_url=...&ap_mac=8c:1e:80:58:cd:80&wlan=WMU%20Guest&statusCode=1
?switch_url=...&ap_mac=00:81:c4:75:63:e0&wlan=WMU%20Guest&statusCode=1
?switch_url=...&ap_mac=2c:36:f8:0d:53:f0&wlan=WMU%20Guest&statusCode=1
```
`statusCode=1` = WLC thinks this client is already logged in. POSTing
would LOOP (see Bug #1 below). Tool should skip POST, re-probe connectivity.

### Variant B (has client_mac, has redirect, no statusCode)
```
?switch_url=...&ap_mac=00:81:c4:75:63:e0&client_mac=e0:e2:58:fd:1f:83&wlan=WMU%20Guest&redirect=detectportal.firefox.com/canonical.html
?switch_url=...&ap_mac=2c:36:f8:0d:65:c0&client_mac=e0:e2:58:fd:1f:83&wlan=WMU%20Guest&redirect=captive.apple.com/
```
Normal initial redirect. Tool should POST `buttonClicked=4` to switch_url.
`redirect_url` POST field = `"http://www.wmich.edu"` + raw rest of redirect
param (matches WMU portal.html's `submitAction()` - yes, this constructs a
malformed URL; WLC ignores the value).

## statusCode values (Cisco AireOS WLC)

Per https://github.com/stuartst/cisco-wlc-captive-portal:

| Code | Meaning |
|---|---|
| (absent) | Normal initial redirect, session state unknown |
| 1 | You are already logged in. No further action required. |
| 2 | You are not configured to authenticate against web portal. |
| 3 | Username already logged in elsewhere. |
| 4 | You have been excluded. |
| 5 | Invalid credentials; retry allowed. |

Only `statusCode=1` causes the loop trap. The others indicate error
states where POST is still appropriate (or pointless, but harmless).

## Form schemas

### portal.html (WMU policy page)
3 hidden inputs. Form has NO `action=` attribute in HTML — set at runtime
by `loadAction()` from the `switch_url` query parameter.

| Field | Default | Purpose |
|---|---|---|
| `buttonClicked` | 0 | Set to 4 by submitAction() on Accept click |
| `redirect_url` | "" | Built from `redirect=` URL param |
| `err_flag` | 0 | 1 if prior auth failed |

### wlc-login.html (Cisco native /login.html)
7 hidden inputs. This is the page the WLC serves if you GET /login.html
directly (outside the WMU-branded flow). Our auth POST now sends all 7
fields to match:

| Field | Default | Required? |
|---|---|---|
| `buttonClicked` | 0 | **yes** (must be `4` for Accept) |
| `err_flag` | 0 | **yes** |
| `err_msg` | "" | no |
| `info_flag` | 0 | no |
| `info_msg` | "" | no |
| `redirect_url` | "" | no (not validated) |
| `network_name` | "Guest Network" | no (display-only) |

## JS deobfuscation notes

The Cisco loginscript.js and WMU's inline scripts are NOT obfuscated —
they're plain readable code. We still run `jsbeautify -d` (19-phase oxc
pipeline) for completeness, but no string rotations / dispatcher tables
/ control-flow flattening are present to unwrap. Outputs in `js-deobf/`
are effectively equivalent to beautified versions.

Notable difference between scripts:

- Cisco `loginscript.js` builds `redirectUrl` via: search `?redirect=`,
  prepend `http://` if missing.
- WMU `portal.html` inline script: search `redirect=`, prepend
  `http://www.wmich.edu`.

Our Rust auth.rs replicates the WMU version (it's what's served in the
WMU flow).

## Cisco WLC quirks observed in the wild

1. **`HTTP 200` with `Location` header** (non-standard) on `/generate_204`.
   We now treat any `Location` header as captive regardless of status.
2. **`GET login.html` before POST** establishes WLC session state. Skipping
   the GET causes POST to return 200 but client MAC isn't promoted.
3. **Self-signed TLS cert** on `virtual.wireless.wmich.edu`. Standard.
4. **DNS hijacking with hostname mismatch** on captive networks —
   `legacy.wmich.edu` may resolve to the WLC IP, which serves
   `virtual.wireless.wmich.edu`'s cert. We set
   `danger_accept_invalid_hostnames(true)` to tolerate.
5. **statusCode=1 loop** — POSTing to an "already logged in" session
   causes WLC to redirect back with statusCode=1 again. We skip POST.
