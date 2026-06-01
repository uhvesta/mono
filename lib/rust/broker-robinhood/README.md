# broker-robinhood

`broker-robinhood` is a low-level async Rust client for Robinhood's
private brokerage HTTP API. It owns one responsibility: speaking
Robinhood's wire protocol — OAuth login with multi-factor / device
approval, and read-only retrieval of accounts, positions, orders, and
instrument symbols — and exposing it as typed Rust calls. It holds no
state of its own beyond an HTTP client and base URLs; callers supply
credentials and access tokens and decide what to do with the results.

## Architecture

The crate centres on `RobinhoodClient`, a cheap-to-clone wrapper around a
`reqwest` client plus two configurable base URLs (the main API host and
the separate identity host that drives login workflows). Constructors
range from a zero-config `new()` that targets Robinhood's production
hosts to variants that inject a custom HTTP client and override base URLs
— the latter exist primarily so tests can point the client at a mock
server.

Authentication is the most involved part of the surface. Robinhood's
login is a multi-step state machine: `initiate_login` posts credentials
and returns an `AuthChallenge` carrying a verification-workflow handle,
and the caller then drives that workflow through the identity host —
advancing entry points, polling push-prompt and verification status, and
completing device-approval challenges — before `finalize_login` exchanges
the now-approved credentials for an access token. The challenge, workflow
route, and screen types modelling these steps live alongside the client
and are re-exported at the crate root.

The remaining methods are read paths that take a bearer access token:
fetching brokerage accounts, walking paginated positions, paging through
equity and option order history with cursor-based pagination, and
resolving instrument IDs to ticker symbols (batched and de-duplicated to
respect server limits). All fallible operations return
`RobinhoodClientError`, a single `thiserror` enum covering URL,
transport, status-code, and body-parse failures.

This is a standalone leaf library with no internal dependencies. The
`hood` crate is its sole internal consumer and layers application logic —
credential handling, persistence, and presentation — on top of this raw
client.
