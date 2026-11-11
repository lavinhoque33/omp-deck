import com.android.build.api.variant.ApplicationAndroidComponentsExtension
import org.gradle.api.file.DirectoryProperty
import org.gradle.api.file.FileSystemOperations
import javax.inject.Inject

plugins {
    alias(libs.plugins.android.application)
}

// The workspace version (Cargo.toml `version = "x.y.z"`) is the app version; the code is major*1_000_000 + minor*1_000 + patch.
val workspaceVersion: Triple<Int, Int, Int> = run {
    val cargo = rootProject.layout.projectDirectory.file("../Cargo.toml").asFile.readText()
    val match = Regex("""(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"""").find(cargo)
        ?: error("no workspace version in Cargo.toml")
    Triple(match.groupValues[1].toInt(), match.groupValues[2].toInt(), match.groupValues[3].toInt())
}

android {
    namespace = "io.github.lavinhoque33.ompdeck"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.lavinhoque33.ompdeck"
        minSdk = 26
        targetSdk = 35
        versionCode = workspaceVersion.first * 1_000_000 + workspaceVersion.second * 1_000 + workspaceVersion.third
        versionName = "${workspaceVersion.first}.${workspaceVersion.second}.${workspaceVersion.third}"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildFeatures {
        buildConfig = true
    }

    // Release signing comes from the environment only (never from the repo): OMPDECK_ANDROID_KEYSTORE (path),
    // OMPDECK_ANDROID_KEYSTORE_PASSWORD, OMPDECK_ANDROID_KEY_ALIAS and optionally OMPDECK_ANDROID_KEY_PASSWORD.
    // Without them the release APK is unsigned (the debug APK uses the Android debug key).
    val keystore = providers.environmentVariable("OMPDECK_ANDROID_KEYSTORE")
    if (keystore.isPresent) {
        signingConfigs {
            create("release") {
                storeFile = file(keystore.get())
                storePassword = providers.environmentVariable("OMPDECK_ANDROID_KEYSTORE_PASSWORD").get()
                keyAlias = providers.environmentVariable("OMPDECK_ANDROID_KEY_ALIAS").get()
                keyPassword = providers.environmentVariable("OMPDECK_ANDROID_KEY_PASSWORD")
                    .orElse(providers.environmentVariable("OMPDECK_ANDROID_KEYSTORE_PASSWORD")).get()
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    installation {
        // Runtime permissions (POST_NOTIFICATIONS) are granted at install, so tests never need UiAutomation for them.
        (installOptions as MutableList<String>).add("-g")
    }

    packaging {
        resources.excludes += "/META-INF/{AL2.0,LGPL2.1}"
    }
}

dependencies {
    implementation(libs.androidx.core)
    implementation(libs.androidx.activity)
    implementation(libs.androidx.webkit)
    implementation(platform(libs.firebase.bom))
    implementation(libs.firebase.messaging)

    // Not used directly: Firebase/Play services pull in androidx.fragment 1.1.0 (via play-services-basement), which
    // lint's InvalidFragmentVersionForActivityResult rejects next to the Activity Result API (needs >= 1.3.0). A
    // constraint raises that transitive version without adding a dependency of our own.
    constraints {
        implementation(libs.androidx.fragment) {
            because("play-services-basement brings fragment 1.1.0; Activity Result APIs need >= 1.3.0")
        }
    }

    testImplementation(libs.junit)
    testImplementation(libs.org.json)

    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.ext)
    androidTestImplementation(libs.androidx.test.core)
}

// ---- bundled PWA ---------------------------------------------------------------------------------------------
//
// The APK carries the phone UI itself (assets/www/): `bun run build.ts --android` in ../pwa produces
// pwa/dist-android, which is synced into the variant's generated assets. Overrides:
//   -Pompdeck.bun=<path to bun>        (default: `bun` from PATH)
//   -Pompdeck.pwaDist=<built dir>      (skip bun; bundle this already-built directory)

val pwaDir = rootProject.layout.projectDirectory.dir("../pwa")
val prebuiltPwa = providers.gradleProperty("ompdeck.pwaDist")

val buildPwa = tasks.register<Exec>("buildPwaAndroid") {
    group = "ompdeck"
    description = "Builds the Android flavour of the PWA with bun."
    onlyIf { !prebuiltPwa.isPresent }
    workingDir = pwaDir.asFile
    commandLine(providers.gradleProperty("ompdeck.bun").getOrElse("bun"), "run", "build.ts", "--android")
    inputs.files(
        fileTree(pwaDir) {
            include("src/**", "public/**", "index.html", "build.ts", "package.json", "bun.lock", "tsconfig*.json")
            exclude("node_modules/**")
        },
    )
    outputs.dir(pwaDir.dir("dist-android"))
}

abstract class SyncPwaAssets : DefaultTask() {
    @get:InputDirectory
    @get:PathSensitive(PathSensitivity.RELATIVE)
    abstract val pwaDist: DirectoryProperty

    @get:OutputDirectory
    abstract val outputDir: DirectoryProperty

    @get:Inject
    abstract val fileSystem: FileSystemOperations

    @TaskAction
    fun sync() {
        val index = pwaDist.file("index.html").get().asFile
        check(index.isFile) { "${pwaDist.get().asFile} has no index.html; build the PWA first" }
        fileSystem.sync {
            from(pwaDist) {
                // Pre-compressed siblings, source maps and the browser-only service worker/manifest stay out of the APK.
                exclude("**/*.br", "**/*.gz", "**/*.map")
            }
            into(outputDir.dir("www"))
        }
    }
}

val syncPwaAssets = tasks.register<SyncPwaAssets>("syncPwaAssets") {
    group = "ompdeck"
    description = "Copies the built Android PWA into the APK assets."
    pwaDist.set(prebuiltPwa.map { rootProject.layout.projectDirectory.dir(it) }.orElse(pwaDir.dir("dist-android")))
    outputDir.set(layout.buildDirectory.dir("generated/pwa-assets"))
    dependsOn(buildPwa)
}

extensions.getByType<ApplicationAndroidComponentsExtension>().onVariants { variant ->
    variant.sources.assets?.addGeneratedSourceDirectory(syncPwaAssets, SyncPwaAssets::outputDir)
}
