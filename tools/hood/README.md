# hood

`hood` is a command-line client for the Robinhood brokerage. It authenticates
a user against Robinhood's login flow, securely caches the resulting OAuth
credentials, and reports read-only account state — account list, connectivity
status, and open positions with live equity and daily-return figures. It is a
standalone developer tool, not part of the Boss automation system.

## How it fits

`hood` is a thin terminal front end over `broker-robinhood`, the sibling
library that owns the actual Robinhood HTTP protocol: the device-approval
login workflow, account and position endpoints, and the typed
`RobinhoodClient`. `hood` contributes the interactive UX around that client —
argument parsing, credential storage, and human-readable rendering — and holds
no protocol knowledge of its own beyond a direct market-data quote lookup used
to price positions.

The binary exposes a handful of subcommands. `auth` drives the multi-step
login (password prompt, push-notification device approval polled to
completion, then token finalization) and persists the OAuth token. `accounts`,
`status`, and `positions` are read-only views over a stored token; `positions`
can follow live (refreshing on an interval), emit CSV, and aggregates equity
and today's return across the selected account(s) into a totals row. Most
commands accept a `--username` (defaulting to the most recently authenticated
user) and an `--account` selector, where `default` resolves to the brokerage's
default account.

Credentials never live in plaintext config: the OAuth token is written to the
macOS system keychain keyed by username, and a small config file (under
`XDG_CONFIG_HOME` / `~/.config`) records the last-authenticated username so
later commands can run without re-specifying it. Verbose diagnostics redact
tokens and other sensitive fields before printing.

Rendering concerns — colored, aligned position tables, currency and percentage
formatting, and the default-account marker — live in the command modules and
are covered by unit tests; the Robinhood wire format is the responsibility of
`broker-robinhood`.
