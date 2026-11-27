# Upgrading this fork from upstream ZGX

The `polyscale/cef` branch carries one product addition on top of upstream
`main`: the optional CEF/Chromium backend (`zgx-desktop` feature `cef`,
living under `crates/zgx-desktop/src/native_host/cef/`, plus the
`cef_web_view` fixture and `docs/cef.md`). Everything else should stay
byte-identical to upstream so rebases stay trivial.

## Rules that keep the fork mergeable

- No product names in ZGX code. The backend derives the app name from the
  executable (`app_name()` in `cef/runtime.rs`) and reads tuning variables
  under generic `ZGX_CEF_*` names. The legacy `POLYSCALE_CEF_*` names are
  accepted as read-only fallbacks (see `cef_env()`) and must never be used by
  new code.
- The page→native activity event uses the reserved origin `native://cef` and
  the body key `"zgx"` (`{"zgx":"native-activity",...}`) — never an app name.
- Internal CDP source URLs use the `zgx://` scheme.
- All CEF code stays behind the `cef` cargo feature; default builds are
  untouched by it.

## Upgrade procedure (run on every upstream release)

```sh
# 0. Record the current upstream base
git fetch origin main
# update UPSTREAM_BASE below to the old origin/main SHA

# 1. Rebase the fork onto the new upstream
git checkout polyscale/cef
git rebase origin/main            # or: git merge origin/main if you prefer merges

# 2. Resolve conflicts — they should only ever touch native_host/cef/,
#    the fixture, docs/cef.md. If a conflict appears anywhere else, the
#    fork drifted: fix the drift, do not merge blindly.

# 3. Verify (macOS arm64)
cargo test -p zgx -p zgx-script -p zgx-desktop --features cef
bash apps/polyscale/scripts/run-cef-gate-macos.sh --mode integration   # from the Polyscale repo, against a temporary local pin

# 4. Push and re-pin Polyscale
#    apps/polyscale/Cargo.toml rev = "<new fork SHA>"

# 5. Update UPSTREAM_BASE
```

## Tracking

`UPSTREAM_BASE` (last upstream `main` SHA merged into this branch):

```
0a18226cfd1512b3154c72659cd12a7f25cf2cd3
```

To measure drift at any time:

```sh
git fetch origin main
git diff --stat $(git merge-base HEAD origin/main)..HEAD -- . ':(exclude)crates/zgx-desktop/src/native_host/cef' ':(exclude)crates/zgx-desktop/examples' ':(exclude)docs/cef.md'
```

Anything outside those paths is drift and should be minimized or upstreamed.
