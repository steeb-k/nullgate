import org.gradle.internal.os.OperatingSystem
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Release signing material is read from a gitignored keystore.properties at the
// android/ root (see docs/android-packaging.md). It is absent on fresh checkouts —
// in that case release builds are produced unsigned (and won't install); debug
// builds are unaffected.
val keystorePropsFile = rootProject.file("keystore.properties")
val keystoreProps = Properties().apply {
    if (keystorePropsFile.exists()) keystorePropsFile.inputStream().use { load(it) }
}

android {
    namespace = "io.github.steeb_k.nullgate"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.steeb_k.nullgate"
        // VpnService is API 14+; we only need a modern baseline. No file-storage
        // permissions, so no API-30 floor (unlike SEED Sync).
        minSdk = 26
        targetSdk = 35
        // BUMP BY HAND EVERY RELEASE — these are literals, NOT derived from the
        // workspace Cargo.toml. Keep versionName == the workspace version; versionCode
        // scheme is MAJOR*10000 + MINOR*100 + PATCH (0.2.0 -> 200, 0.2.1 -> 201), so it
        // stays monotonic and decodable. A stale versionCode blocks in-place updates.
        // See docs/releasing.md + docs/android-packaging.md.
        versionCode = 203
        versionName = "0.2.3"
    }

    signingConfigs {
        if (keystorePropsFile.exists()) {
            create("release") {
                storeFile = rootProject.file(keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            // Sign with the release key when keystore.properties is present;
            // otherwise the APK is left unsigned (won't install — by design).
            if (keystorePropsFile.exists()) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
    buildFeatures {
        compose = true
    }
    // The Rust .so files are produced by cargo-ndk into src/main/jniLibs.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")
    // UniFFI-generated Kotlin is written into the default source root
    // (src/main/java/uniffi/...) by the uniffiBindgen task, so it compiles like
    // any other source without dynamic generated-source registration.

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    implementation("androidx.lifecycle:lifecycle-service:2.8.7")
    implementation("androidx.activity:activity-compose:1.9.3")

    val composeBom = platform("androidx.compose:compose-bom:2024.10.01")
    implementation(composeBom)
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")

    // QR scanning for "join via ticket" (camera -> ticket). Standalone (no Play
    // Services), fits the sideload/F-Droid distribution.
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")

    // Required at runtime by the UniFFI-generated Kotlin bindings.
    implementation("net.java.dev.jna:jna:5.15.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    // Kotlin support class that the Rust rustls-platform-verifier calls back into
    // for Android trust-store validation (iroh relay TLS). Resolved from the
    // local Maven repo wired up in settings.gradle.kts.
    implementation("rustls:rustls-platform-verifier:0.1.1")

    debugImplementation("androidx.compose.ui:ui-tooling")
    implementation("androidx.compose.ui:ui-tooling-preview")
}

// ---------------------------------------------------------------------------
// Rust build: compile crates/ipn-mobile to per-ABI .so via cargo-ndk, then
// generate the UniFFI Kotlin bindings from the built library.
// ---------------------------------------------------------------------------

val workspaceRoot = rootProject.projectDir.parentFile // android/ -> repo root
val jniLibsDir = file("src/main/jniLibs")
// Generated UniFFI Kotlin is written under the default source root so it
// compiles without dynamic generated-source registration. The uniffi/ subtree is
// gitignored — regenerated by the uniffiBindgen task on every build.
val uniffiOutDir = layout.projectDirectory.dir("src/main/java")

// ABIs to build, mapped to their Rust target triples. arm64 is primary (modern
// phones); armeabi-v7a for older 32-bit devices; x86_64 for the emulator.
val abiToTriple = mapOf(
    "arm64-v8a" to "aarch64-linux-android",
    "armeabi-v7a" to "armv7-linux-androideabi",
    "x86_64" to "x86_64-linux-android",
)

fun cargo(vararg args: String): List<String> =
    if (OperatingSystem.current().isWindows)
        listOf("cmd", "/c", "cargo", *args)
    else
        listOf("cargo", *args)

// Build (without cargo-ndk's `-o`, which over-copies intermediate dependency
// dylibs from deps/) then copy only the self-contained libipn_mobile.so per ABI.
val cargoNdkBuild by tasks.registering(Exec::class) {
    group = "rust"
    description = "Cross-compile ipn-mobile to Android .so via cargo-ndk"
    workingDir = workspaceRoot
    val ndk = (findProperty("ndkDir") as String?) ?: System.getenv("ANDROID_NDK_HOME")
    if (ndk != null) environment("ANDROID_NDK_HOME", ndk)

    val abiArgs = abiToTriple.keys.flatMap { listOf("-t", it) }
    commandLine(
        cargo(
            "ndk",
            *abiArgs.toTypedArray(),
            "build", "--release", "-p", "ipn-mobile"
        )
    )
    doLast {
        abiToTriple.forEach { (abi, triple) ->
            val built = File(workspaceRoot, "target/$triple/release/libipn_mobile.so")
            val destDir = File(jniLibsDir, abi).apply { mkdirs() }
            built.copyTo(File(destDir, "libipn_mobile.so"), overwrite = true)
        }
    }
}

// Build the host cdylib so the binding generator can read it. UniFFI's
// `--library` metadata extraction can't parse a foreign-arch (Android) .so on a
// Windows host, and UniFFI metadata is platform-independent anyway — the Kotlin
// generated from the host library is identical to what the Android .so would
// yield, and the runtime checksum guards still match since both are built from
// the same source + UniFFI version.
val cargoHostBuild by tasks.registering(Exec::class) {
    group = "rust"
    description = "Build the host ipn-mobile cdylib for UniFFI binding generation"
    workingDir = workspaceRoot
    commandLine(cargo("build", "-p", "ipn-mobile", "--lib"))
}

val hostLibName = when {
    OperatingSystem.current().isWindows -> "ipn_mobile.dll"
    OperatingSystem.current().isMacOsX -> "libipn_mobile.dylib"
    else -> "libipn_mobile.so"
}

val uniffiBindgen by tasks.registering(Exec::class) {
    group = "rust"
    description = "Generate Kotlin UniFFI bindings from the host ipn-mobile library"
    dependsOn(cargoHostBuild)
    workingDir = workspaceRoot
    val hostLib = File(workspaceRoot, "target/debug/$hostLibName")
    val outDir = uniffiOutDir.asFile
    doFirst { outDir.mkdirs() }
    commandLine(
        cargo(
            "run", "-p", "ipn-mobile", "--features", "bindgen", "--bin", "uniffi-bindgen", "--",
            "generate", "--library", hostLib.absolutePath,
            "--language", "kotlin", "--out-dir", outDir.absolutePath
        )
    )
}

// Wire the Rust build ahead of Kotlin compilation/packaging: bindgen produces the
// Kotlin sources, cargoNdkBuild produces the per-ABI .so into jniLibs.
tasks.named("preBuild") {
    dependsOn(uniffiBindgen, cargoNdkBuild)
}
