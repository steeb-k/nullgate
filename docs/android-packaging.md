# Android packaging & build

Nullgate's Android app lives in `android/` (a standard Gradle/Android Studio project) and wraps
the Rust engine through the `ipn-mobile` UniFFI facade. It runs `ipn-core` **in-process** inside a
foreground service — there is no separate daemon on Android — and routes packets through Android's
`VpnService`.

## Architecture on Android

```
Kotlin/Compose UI ─┐
Foreground service ─┼─► MobileEngine (UniFFI, crates/ipn-mobile, owns a tokio runtime)
VpnService (TUN fd) ┘        │  block_on
                            ▼
                     ipn_core::Engine  (same engine as desktop; fd-injection path)
                            │
                     iroh endpoint / docs / gossip ──► relays + LAN (mDNS)
```

- **`crates/ipn-mobile`** — `cdylib` exposing `MobileEngine` + an `EventListener` callback over
  UniFFI 0.28 (proc-macro mode). The Gradle build generates Kotlin bindings from the **host**
  library (UniFFI metadata is platform-independent) and cross-compiles the per-ABI `.so`.
- **Routing.** Desktop opens its own TUN; Android can't (only `VpnService` may), so the engine
  emits `EngineEvent::TunSetupRequired { ip, mtu }`; the app builds the `VpnService`
  (`addAddress(ip, 24)`, `addRoute(10.99.0.0, 24)`, `setMtu`) and hands the resulting fd to
  `Engine::attach_tun_fd` via `ParcelFileDescriptor.detachFd()`. **Split tunnel**: only the
  `10.99.0.0/24` is routed, so iroh's own relay/peer traffic (public IPs) bypasses the tunnel and
  no `VpnService.protect()` is needed.
- **Device name.** The OS hostname is meaningless on Android and the hardware serial is
  unreachable to normal apps, so the app auto-derives `"<Manufacturer> <Model> (<suffix>)"`
  (suffix from `Settings.Secure.ANDROID_ID`) and passes it to `set_device_name_override`. It's not
  user-editable; the NodeId remains the real identity anchor.
- **Secrets** are file-backed under app-private internal storage (`keyring` has no Android
  backend). The **geolocation** stack is not compiled on Android.

## Toolchain

- **JDK 17** (`JAVA_HOME`), **Android SDK 35** (`ANDROID_HOME`), **NDK r27c** (`ANDROID_NDK_HOME`).
- `cargo install cargo-ndk`; `rustup target add aarch64-linux-android armv7-linux-androideabi
  x86_64-linux-android`.
- On Windows, turn **Smart App Control off** (it blocks cargo build scripts: os error 4551).

## Build & run

```powershell
# Build + install + launch on the emulator (AVD seed_api35), or a USB device with -Device.
pwsh -File scripts\run-android.ps1
pwsh -File scripts\run-android.ps1 -Device
pwsh -File scripts\run-android.ps1 -Release   # needs android\keystore.properties
```

Or directly with Gradle:

```sh
cd android
./gradlew :app:assembleDebug      # first build cross-compiles 3 ABIs (~10 min)
adb install -r app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n io.github.steeb_k.nullgate/.MainActivity
adb logcat -s nullgate            # engine logs (tracing -> Logcat)
```

The Gradle `preBuild` runs three Rust tasks: `cargoNdkBuild` (per-ABI `libipn_mobile.so` into
`src/main/jniLibs/`), `cargoHostBuild` (host cdylib for binding metadata), and `uniffiBindgen`
(Kotlin into `src/main/java/uniffi/`). Both `jniLibs/` and the generated `uniffi/` subtree are
gitignored — regenerated every build.

## ABIs, versioning, signing

- ABIs: `arm64-v8a` (primary), `armeabi-v7a` (older 32-bit), `x86_64` (emulator) — one universal APK.
- `versionCode = MAJOR*10000 + MINOR*100 + PATCH` (0.2.0 → 200); `versionName` tracks the
  workspace version.
- Release signing reads `android/keystore.properties` (gitignored — **never commit; a lost signing
  key permanently blocks in-place updates**). Without it, release builds are produced unsigned
  (won't install); debug builds are unaffected.
- `minSdk 26`, `targetSdk 35`, `compileSdk 35`. No file-storage permissions; the only runtime
  permissions are the VPN consent (system dialog), notifications (API 33+), and camera (optional,
  for QR scanning).

Builds are **local** (no CI), consistent with the rest of the project.
