# Z·POOL (Windows on ARM fork)

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Tauri](https://img.shields.io/badge/Tauri-2.x-24C8DB?logo=tauri&logoColor=white)](https://tauri.app)
[![Release](https://img.shields.io/github/v/release/Kepler16f/zcode-pool-WOA?label=release)](../../releases)

[简体中文](README.md) ｜ **English**

Z·POOL is a Tauri 2 desktop application that keeps ZCode / Z.ai account identities, an Outlook mailbox pool and
per-account quotas in one place — and exposes a local proxy with a **web console** so third-party apps such as
pi-agent or LobeChat can spend the pool's quota directly.

## About this fork

This repo is a fork of [Limitinfinitude/zcode-pool](https://github.com/Limitinfinitude/zcode-pool), named **zcode-pool-WOA** (Windows on ARM).
All upstream features are kept; three fixes target ARM64 Windows devices (Snapdragon X, Surface Pro 9/11, etc.) and real-world setups:

| Change | The upstream problem | What this fork does |
| --- | --- | --- |
| **Native Windows ARM64 support** | CI only produced `*_x64-setup.exe`; ARM64 devices had to run it through x64 emulation | Added an `aarch64-pc-windows-msvc` cross-compile target shipping `*_arm64-setup.exe`; bumped `cc` so MSVC is discovered on ARM64 hosts; the gateway's `x-platform` header now reports the real architecture (unchanged on x64) |
| **International sign-in fix** | The sign-in proxy only covered the login window; but the `oauth/cli/init` request that decides which login page you get runs in Rust and was **direct**, so a mainland exit IP always received the mainland `authorize_url` (phone-number login only) | The sign-in proxy now covers the whole OAuth flow (authorize init / login window / token exchange); when no explicit proxy is set it follows the Windows system proxy (Internet Settings registry); SOCKS5 proxies are now supported (HTTP only before) |
| **ZCode install-path fix** | The relay 503s locally when it can't find `resources\glm\zcode.cjs` — the x64 ZCode build installs to `C:\Program Files (x86)\ZCode\` on ARM64 machines, which wasn't probed | Added the `(x86)` path to `client_path_candidates`; account switching benefits the same way |

Local distribution (`npm run dist`) was adapted too: it reads cross-compiled `target/<triple>/release` outputs, detects the
architecture from the PE header, and suffixes non-x64 portable builds with `_arm64` so they can't be confused with x64 ones.

## Screenshots

**Web console** (open `http://127.0.0.1:8899/proxy`; page edits need no rebuild)

| Dashboard | Channel health |
| :---: | :---: |
| ![Dashboard](assets/dashboard.png) | ![Channel health](assets/channels.png) |

| Usage | Account pool |
| :---: | :---: |
| ![Usage](assets/usage.png) | ![Account pool](assets/accounts-console.png) |

**Desktop panel** (mailbox pool / account library)

| Mailbox pool | Account library and quotas |
| :---: | :---: |
| ![Mailbox pool](assets/mailbox.png) | ![Account library and quotas](assets/accounts.png) |

## Features

### proxy gateway + web console

- Serves an Anthropic-protocol endpoint (`http://127.0.0.1:8899/v1/messages`) that clients can point straight at the pool.
- **Ships its own console**: relay on/off, listen address, port, API keys, account-selection policy, model policy and
  model mapping are all editable from the browser — no need to go back to the desktop panel.
- **Switchable listen address**: loopback only by default. Binding to a LAN-reachable address **requires at least one
  API key first**, otherwise anyone on the same subnet can burn your quota. Auth rule: loopback is exempt,
  remote callers must present a key.
- **Model policy**: `auto fallback` (when the requested model has no quota anywhere, use another one that does) or
  `pinned` (lock one model; no switching even when exhausted).
- **Model mapping**: rename or retire a model at the gateway instead of editing every client.
- **Quota cache on disk**: quotas no longer live only in memory, so a restart doesn't re-query everything.
- **Configurable outbound proxy for the relay**: the relay does not read the system proxy. When the egress IP is
  flagged (`3012`), set an http / socks5 proxy in Settings instead of keeping a global TUN on.

### Console pages

| Page | What it does |
| --- | --- |
| Dashboard | Service state, currently routed account, pool throughput, TTFB / total latency, 60-sample sparkline, probe-latency and request-outcome distributions |
| Channels | One row per account: state, latency, recent-history sparkline, success rate, failure streak, last probe; probe / unfreeze per row, plus a pool-wide latency summary (mean / median / P95 / sample count) |
| Usage | Today's and lifetime tokens (input / output / cache broken out), success rate and cache-hit share, grouping by model / account / key, filterable and paginated request log |
| Accounts | One row per account with status and name filters plus paging; ring-gauge quota dashboard, quotas expiring today, sync-from-login, launch / kill ZCode |
| Mailbox | Import / export the mailbox pool, batch verification, status filter |
| Settings | Relay toggle / listen address / port / API keys / outbound proxy / ZCode path / selection policy / model policy / model mapping |
| Logs | Raw runtime log with paging |

The console also serves a **chat page** (`http://127.0.0.1:8899/`) for verifying the relay end to end, with streaming
output and collapsible reasoning.

### Account library

- Store multiple login identities and switch between them in one click. The current login is archived before every
  switch, so no account is ever lost.
- Supports both BigModel and z.ai sign-in. Adding an account never disturbs the login you are using.
- Can relaunch ZCode automatically after a switch.

### Quotas and plans

- Aggregates remaining quota across all accounts, grouped by model, with plan, quota window, reset time and expiry.
- Checks which plans are claimable, and claims them manually or on a fixed schedule.

### Mailbox pool

- Imports `email----password----client_id----refresh_token` files and reads Outlook / Hotmail verification mail.
- Runs signup, mail pickup, verification and authorization in batches. The slider CAPTCHA is completed manually.
- Supports re-authorization, credential updates and status filtering.

## Installation

### Prebuilt binaries

Download the package for your platform from the [Releases](../../releases) page:

| Platform | File |
| --- | --- |
| Windows | `*_x64-setup.exe` |
| Windows (ARM64) | `*_arm64-setup.exe` |
| macOS | `*_universal.dmg` |
| Linux | `*.deb` / `*.AppImage` |

The binaries are not code-signed, so the system may warn about an unknown publisher on first launch.

### Building from source

Requirements: Node.js 18 or newer and stable Rust. On Windows the default MSVC toolchain works for both x64 and ARM64 devices. ARM64 devices additionally need LLVM installed with `clang` on PATH — `ring` compiles its C code with clang on Windows ARM64, and MSVC's cl.exe cannot be used there.

```powershell
npm install
npm run build
Set-Location src-tauri
cargo build --release
```

The binary is written to `src-tauri/target/release/zcode-pool.exe`. To also produce the portable build and the installer:

```powershell
npm run dist
```

After editing i18n strings, `npm run check:i18n` verifies that the Chinese and English tables stay in sync.

## Usage

1. **Add an account** — in the account library, click *Add* and sign in through the window that opens. The account is stored automatically and your current login is untouched.
2. **Switch accounts** — click *Switch* on a row. ZCode is closed and restarted so the new login takes effect.
3. **Verify mailboxes** — in the mailbox pool, click *Import txt*, tick the mailboxes you want, then click *Auto-verify*. Each account needs one manual slider check.
4. **Claim plans** — click *Refresh* to see what each account can claim, then *Claim* manually or enable *Auto claim* to check on a fixed interval.
5. **Point a client at it** — confirm the relay is on in Settings, then use `http://127.0.0.1:8899` as the base URL (Anthropic protocol).
   Loopback callers need no key; to reach it from another machine on the LAN, switch the listen address and create an API key.

## Data and security

- Accounts, mailboxes and settings live under `~/.zcode-pool/`. Everything stays local.
- `mail.json` stores mailbox passwords and refresh tokens in plain text. Do not commit it or share it.
- Exported accounts contain only z.ai / BigModel credentials, provider config and the device identity. Third-party provider keys, SSH passwords and relay credentials are stripped.
- The relay listens on `127.0.0.1` only by default. Binding it to a LAN-reachable address **requires** at least one API key, or the listener is refused.
- Usage records keep only the first 8 characters of an API key — never the full secret.
- Logs are written to `%LOCALAPPDATA%\com.zpool.app\logs\oauth.log`.

## Disclaimer

This project is shared for technical learning only. Please comply with the relevant terms of service and laws, and use it at your own risk.

## Credits

Parts of the code and design reference these MIT projects:

- [zcode-switch](https://github.com/pjpv/zcode-switch)
- [OutlookEmail](https://github.com/assast/outlookEmail)

## License

[MIT](LICENSE)
