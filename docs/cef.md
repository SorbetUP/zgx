# CEF backend

Polyscale's Chromium backend is an optional ZGX desktop feature (`cef`). It
uses CEF 156.7.0 with windowless `OnPaint` rendering and keeps wgpu 29. The
CEF runtime, helper processes, resources, and sandbox files must be bundled
beside the app using the platform-specific layout documented by
`bundle-cef-app`.

## Local macOS fixture

Build and bundle the fixture with the repository's `cef-package` feature:

```sh
cargo run -p zgx-desktop --features cef-package --bin bundle_cef_fixture -- /tmp/cef-fixture
```

Automated fixture runs use CEF's mock keychain through the test-only
`use-mock-keychain` switch. This avoids reading or modifying the developer's
login keychain. The fixture enables this when its test environment variable is
present; the production Polyscale bundle does not set the switch.

Production CEF retains Chromium's normal macOS Safe Storage behavior for
persistent browser data. The system keychain prompt is macOS asking whether the
signed app may access Chromium's encryption key. The fixture's mock keychain
must not be used for real user profiles because it is for automated tests only.
