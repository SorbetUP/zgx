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

The `cef_web_view` fixture automatically enables CEF's mock keychain through
the test-only `use-mock-keychain` switch and uses a per-run temporary data
directory. It does not need access to the developer's login keychain, so its
launch should not require entering the keychain password. This only applies to
the fixture executable; production Polyscale does not set the switch.

To exercise closing a controller while its browser is still being created,
set `ZGX_CEF_CLOSE_RACE_RESULT` to a result-file path before launching the
fixture. The fixture closes the page at mount and writes `PASS` only after the
desktop event loop finishes its normal CEF shutdown.

Set `ZGX_CEF_NAV_EVAL_RACE_RESULT` to test that an evaluation's pending
JavaScript promise is rejected when its document navigates away before the
promise resolves. The expected result is `PASS` in the selected file.

Production CEF retains Chromium's normal macOS Safe Storage behavior for
persistent browser data. macOS may ask once whether the signed app may access
Chromium's encryption key; this is required for encrypted persistent storage.
Choosing “Always Allow” saves that decision for the app's signing identity. A
recompiled unsigned development fixture may have a different identity each
time, which is why the fixture uses mock Safe Storage. That mock must not be
used for real user profiles because it does not provide the system keychain's
protection.

CEF file input dialogs are routed through ZGX's native file chooser. Open-one,
open-many, and folder selection are supported. Save-dialog requests are
cancelled; downloads use the separate explicit download flow.
