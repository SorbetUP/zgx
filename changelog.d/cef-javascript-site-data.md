---
section: fixed
---
- **CEF pages can return JavaScript results, send WebView messages, and clear one site's data** (zgx-desktop): `WebView::evaluate_js`, configured user scripts, and both ZGX message bridge spellings now work through CEF without an exposed DevTools port. Website-data removal uses Chromium's origin-scoped storage command. The packaged macOS smoke fixture is ad-hoc signed so repeated local runs are not killed by macOS after its bundle changes.
