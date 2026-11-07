# ADR 0009: How macOS-only native code is allowed

- **Status:** Proposed
- **Date:** 2026-11-08

## Context

The macOS app in `desktop/app/src/mac/` is mostly stubs. Most Apple APIs for the missing features are Objective-C or Swift only. The workspace sets `unsafe_code = "deny"`, so macOS FFI is hand-declared `extern` blocks with `#[allow(unsafe_code)]` (see `mac/input/cg.rs` on `feat/macos-input` and `mac/h264/vt.rs`). `CONTRIBUTING.md` says "One bridge. Only `desktop/app/src/bridge` (cxx-qt) connects Rust to Qt." The project has no macOS crates. A new crate needs an ADR and a `deny.toml` check.

What the designs found:

- Public C APIs cover many areas: SystemConfiguration, `getifaddrs`, AudioToolbox (`AudioQueue`, `AudioConverter`), CoreAudio (`AudioObject`, aggregate devices, `IOProc`), CoreGraphics (`CGWindowListCopyWindowInfo`, permission preflight), HIServices (Accessibility), IOKit power, `libproc`, CoreFoundation. These need no Objective-C.
- Some APIs have no C entry point: `NSSharingServicePicker`, the Services provider, `NSEvent` for media keys, `CATapDescription` (the tap header is inside `#ifdef __OBJC__`), `NSWorkspace` and `NSRunningApplication`, CoreWLAN, CoreBluetooth, ScreenCaptureKit. They can be reached with the public Objective-C runtime C functions (`objc_getClass`, `sel_registerName`, `objc_msgSend`, `objc_allocateClassPair`, `class_addMethod`).
- `cpp/app_helpers.cpp` is already C++ compiled inside the cxx-qt build. It already calls `objc_msgSend` for `NSPasteboard` under `#ifdef __APPLE__` (about lines 379 to 530). It is the only Objective-C runtime use in the repo.
- Open upstream draft PRs show three other routes. Drafts 62 and 64 add `objc2`, `objc2-app-kit`, `objc2-foundation` and `dispatch2` (draft 64 also `objc2-user-notifications`). Draft 63 compiles `.m` files with `cc` and `xcrun` in `build.rs` behind a `macos-native-camera` feature and builds a camera extension.
- Some features cannot live in the main binary at all: a Share extension, a WidgetKit widget, a File Provider, a camera extension and an audio Server plug-in are separate signed bundles.
- The shipped bundle (`desktop/packaging/macos/build-app.sh`) is ad-hoc signed with `codesign --force --deep --sign -`. It has no hardened runtime, no entitlements file and no notarization. The plist bundle id is `app.nectarlink.desktop`; `mac/autostart.rs` and `mac/toast.rs` use `com.thtbee.nectarlink`.

## Decision

Allow macOS-only native code in three levels. Use the lowest level that works.

1. **Hand-declared C FFI in Rust (default).** Use it for every public C API. Rules: `#[allow(unsafe_code)]` only in the file that declares the `extern` block; cite the header path in the file comment; link frameworks with `#[link(name = "...", kind = "framework")]`; wrap every Create-rule object (`CFRelease`) in a drop guard; copy Get-rule values before release; handle NULL returns; never keep a pointer across calls; resolve symbols newer than the 14.0 minimum with `dlsym` and a parsed OS version (a failed version lookup means "not allowed"). No new crate.
2. **Objective-C runtime calls, only where no C API exists.** Two allowed forms, in this order:
   - Typed `objc_msgSend` casts. Either from Rust (hand-declared, one function pointer type per selector, struct arguments such as `NSPoint` and `NSRect` by value with the exact layout) or from the existing `cpp/app_helpers.cpp` under `#ifdef __APPLE__` (the NSPasteboard precedent). Both stay inside the one cxx-qt build and add no crate and no new bridge. A runtime-created delegate class (`objc_allocateClassPair`) is allowed in the same places.
   - Objective-C++ (`.mm`) in `desktop/app/cpp` behind cxx-qt, only if typed message sends become unreasonable for a feature (for example delegate-heavy CoreBluetooth). A `.mm` file is C++ compiled in the cxx-qt build, so it is not a second bridge, but it is a new language file type. Needs a PR-level note and the build and header checks below.
   Rules for both: the Objective-C code never calls back into Qt types, only into Rust through a plain C callback; it handles autorelease objects (the autorelease-pool primitives are not in the public SDK, so use `NSAutoreleasePool` through message sends or an existing pool); it never logs content; every selector and cast is covered by a hardware check on both architectures.
   The `objc2` crates (drafts 62 and 64) are an alternative to typed casts. Accept them only if one of those drafts merges first and passes the `deny.toml` check, then reuse them. Do not add a second set.
3. **Swift or Xcode app extensions (Phase 4 only).** A Share extension, a WidgetKit widget, a camera extension (draft 63), a File Provider and an audio Server plug-in are separate bundles. Each needs its own ADR, a team ID, signing of every nested bundle (the `--deep` call must be replaced), an app-group or URL hand-off to the main app, and a decision on distribution. A `.m` file compiled with `cc` as in draft 63 is allowed for extension bundles. For code linked into the main binary, use the `.mm` form of level 2 instead, so there is one build route.

Rejected: a second cxx-qt bridge, Qt Bluetooth or Qt Multimedia for macOS-only features, `IOHIDPostEvent` (deprecated), `AuthorizationExecuteWithPrivileges` (deprecated 10.7), the private `MediaRemote` framework, `__AXUIElementGetWindow` (private, links but is not declared), `CGSEventTap` (not public), the obsoleted CoreGraphics screen capture calls.

### Bundle id and provisioning

- Pick one bundle id before any extension, app group, TCC instruction or notification identity is written. Today there are two (`app.nectarlink.desktop` in the plist, `com.thtbee.nectarlink` in code).
- A fork must use its own bundle id prefix and its own team. An extension id must be prefixed by the host app id. An app group on macOS is expected to need a team-prefixed id (unverified: the Apple page returned only a title). An ad-hoc signature has no team ID, so app groups, pluginkit registration and Services registration with an ad-hoc bundle are unknown until a Mac test.
- Apple's extension guide (read during research) says the containing app and every extension must be signed the same way, that ad-hoc is allowed for testing, and that Gatekeeper blocks extensions distributed outside the Mac App Store until the user opens the containing app. Whether sandboxing is mandatory is not stated outright (templates default to App Sandbox).
- TCC grants are tied to the code identity. An ad-hoc cdhash changes on every build, so a grant may be lost on update. Apple does not document this. It is a hardware check for every permission-bearing PR and a possible trigger for Developer ID.
- Hardened runtime and notarization are not enabled today. If `--options runtime` is added, re-test every "needs no entitlement" claim: audio capture, AudioToolbox, runtime class creation, Qt plugin loading under library validation. Unknown until tried.
- User-facing plist strings are outside `tr!`. Provide per-language `InfoPlist.strings` or accept English only and say so. `CONTRIBUTING.md` requires translatable text, so this is an open conflict to settle in the first PR that adds a key.

### Header and build checks

- `scripts/check-headers.ps1` checks the SPDX header only for `.rs .qml .cpp .h .hpp .ps1 .sh .kt .kts`. If `.m`, `.mm` or `.swift` files appear, add them to `$extensions` in the same PR. If `.plist` or `.entitlements` files are added they cannot carry a header comment the same way: decide whether to add them to the list or exclude them.
- `desktop/app/build.rs`: a `.mm` file would have to be added to the `cpp_files` list for the cxx-qt build. Unknown whether `cxx-qt-build` accepts `.mm` there. Draft 63 shows the `cc` route instead. Prove one before the first `.mm` PR.
- `deny.toml`: no change for levels 1 and 2 without a crate. `objc2*`, `dispatch2` or any `objc2-core-*` crate needs a check and an ADR note (licence, version). `objc2-core-bluetooth` is not in the lock file; `objc2-core-wlan` is a separate crate that was not checked.
- CI: a macOS job must build and run the pure tests. Framework-backed tests are marked and run on macOS CI only, never on a desktop session that raises a permission prompt.

## Consequences

- Most of the macOS plan (network, wake info, Doctor reads, window list, AudioQueue, AudioConverter, MP4 muxer, power events) stays in plain Rust with no new crate.
- The Objective-C runtime is allowed in a narrow way, so the one-bridge rule keeps its meaning: Rust talks to Qt only through cxx-qt, and Apple objects only through the runtime inside the same build.
- Typed `objc_msgSend` is fragile on arm64 and x86_64. Each use needs both architectures tested. If more than a few sends are needed in one feature, prefer `objc2` (if already in the tree) or a small `.mm` helper.
- Phase 4 features cost a Developer ID, a team, nested signing, an app-group or URL hand-off and a distribution plan. Nothing in this ADR approves them. Each gets its own ADR.
- Every PR must list the hardware checks that prove its native calls, because Apple documents many details only loosely.

## Which areas force the choice

| Area | Forces | Why |
|---|---|---|
| Wi-Fi, wake, network watcher, Doctor, window list, AX, PC audio playback, MP4 AAC, power events | Level 1 | Public C APIs only |
| Media keys | Level 2 | `NSEvent` has no C route; CoreGraphics has no public system-defined event |
| Share picker, Services provider, Dock menu hook, foreground tracking | Level 2 | AppKit classes and a runtime-created provider; the existing C++ file is the precedent |
| PC audio tap | Level 1 for the C calls, level 2 for `CATapDescription` | Tap header is `#ifdef __OBJC__`; objc2 if already present |
| Window and screen capture (deferred) | Level 2 (typed sends or a `.mm` helper) | ScreenCaptureKit has no C API; blocks and a delegate protocol |
| Wi-Fi join (deferred) | Level 2 plus CoreLocation | CoreWLAN is Objective-C only |
| BLE beacon (deferred) | Level 2, delegate heavy | CoreBluetooth delegates; may justify `.mm` or objc2. Also blocked by protocol, not only by language |
| Share extension, WidgetKit, camera extension, File Provider, virtual microphone (deferred) | Level 3 | Separate signed bundles; WidgetKit is Swift only |

## Unknown until hardware

- Whether `cxx-qt-build` compiles `.mm` files.
- Whether typed message sends from a worker thread are safe for `NSEvent`, `NSSharingServicePicker` and `NSWorkspace` (main-thread rules are not stated in the headers).
- Whether runtime class creation and `class_replaceMethod` on Qt's delegate work with Qt 6.12.0.
- Whether app groups, pluginkit and Services registration work with an ad-hoc signature.
- Whether TCC grants survive an ad-hoc rebuild.
- Whether `--options runtime` needs any entitlement for the features above.
