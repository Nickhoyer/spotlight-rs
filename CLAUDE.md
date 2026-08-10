# Spotlight-rs

## Code signing and releases

### Why the signing identity matters

macOS records a *designated requirement* when the user grants a TCC permission —
here, Accessibility, which `crates/platform-macos/src/input.rs` needs to post the
synthetic ⌘V. An ad-hoc signature has no certificate to anchor that requirement
to, so it falls back to the cdhash. Every rebuild changes the cdhash, macOS reads
the new build as a different app, and the grant is silently dropped.

A Developer ID signature anchors the requirement to the team certificate:

```
identifier "com.nickolashoyer.spotlight-rs" and anchor apple generic
  and certificate leaf[subject.OU] = U8985K4P8C
```

No cdhash, so every future build satisfies it and the grant survives updates.

**Do not change `CFBundleIdentifier` or the Team ID.** Either one resets user
permissions exactly the way ad-hoc signing did. Certificate *renewal* is safe —
the Team ID carries across.

### Building

`scripts/bundle.sh` signs ad-hoc unless `SIGN_IDENTITY` is set, so an
unconfigured checkout still builds. A build reporting `Signature=adhoc` means the
identity isn't configured, not that something is broken.

```bash
# ad-hoc — fine for local iteration, but resets permissions on every rebuild
./scripts/bundle.sh

# signed
SIGN_IDENTITY="Developer ID Application: NICKOLAS HOEYER LARSEN (U8985K4P8C)" ./scripts/bundle.sh

# signed + notarized + stapled (what a release does)
SIGN_IDENTITY="Developer ID Application: NICKOLAS HOEYER LARSEN (U8985K4P8C)" \
NOTARY_PROFILE=spotlight-notary ./scripts/bundle.sh
```

`NOTARY_PROFILE` refers to a keychain profile created once with
`xcrun notarytool store-credentials`. Notarization adds ~30s.

Two things the script does that are easy to get wrong if it's ever rewritten:

- **Signs inside-out** (nested binary, then the bundle) rather than `--deep`,
  which is deprecated and applies the outer options to inner code.
- **Re-zips after stapling.** The ticket is written into the `.app`, so the
  archive submitted for notarization is *not* the one to ship.

`packaging/spotlight-rs.entitlements` is deliberately empty. The hardened runtime
(required for notarization) doesn't block anything this app does — Accessibility
is a TCC grant, not an entitlement, and Metal runtime shader compilation is fine.
Add entitlements only if a signed build fails at launch; the file lists the
likely candidates.

### CI

`.github/workflows/release.yml` releases on push to main when there are
Conventional-Commit changes warranting one. It imports the certificate into a
throwaway keychain and signs + notarizes when these repo secrets exist, and falls
back to an ad-hoc build when they don't:

| Secret | What it is |
| --- | --- |
| `MACOS_CERT_P12` | base64 of the exported Developer ID `.p12` |
| `MACOS_CERT_PASSWORD` | password set when exporting that `.p12` |
| `MACOS_SIGN_IDENTITY` | `Developer ID Application: … (U8985K4P8C)` |
| `NOTARY_KEY_P8` | base64 of the App Store Connect API key `.p8` |
| `NOTARY_KEY_ID` | that key's Key ID |
| `NOTARY_ISSUER` | team Issuer ID |

The build step runs *before* tagging and publishing, so a signing failure kills
the job without producing a release.

### Homebrew tap

The cask users install comes from the **`Nickhoyer/homebrew-tap`** repo. CI bumps
only its `version` and `sha256`. `packaging/spotlight-rs.rb` is a reference copy
that reaches nobody — keep the two in structural sync by hand.

Releases have been notarized since v0.10.0, so the cask no longer strips
`com.apple.quarantine`. A stapled build clears Gatekeeper with the attribute
still present.

### Debugging a permission reset

Check the signature before looking at app code:

```bash
codesign -dvvv /Applications/Spotlight-rs.app 2>&1 | grep -E 'TeamIdentifier|Signature'
```

`TeamIdentifier=not set` means an ad-hoc build got installed. If the signature is
correct but permission still fails, suspect a stale TCC row — the symptom is
System Settings showing the toggle **on** while the app is denied:

```bash
tccutil reset Accessibility com.nickolashoyer.spotlight-rs
```

Then restart the app (trust status is cached per-process) and re-grant once.
