import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Release signing. The keystore and its passwords live outside the repository;
// keystore.properties points at them and is never committed.
val keystoreProperties = Properties().apply {
    val file = rootProject.file("keystore.properties")
    if (file.exists()) {
        file.inputStream().use { load(it) }
    }
}
val releaseStoreFile = keystoreProperties.getProperty("storeFile")?.let(::File)
val hasReleaseSigning = releaseStoreFile?.exists() == true

// The workspace Cargo.toml is the one place a version is written. Declaring it
// again here is how the 2.0.5 APK came to call itself 2.0.4.
val workspaceVersion: String = rootProject.file("../../Cargo.toml")
    .readLines()
    .dropWhile { !it.startsWith("[workspace.package]") }
    .first { it.trimStart().startsWith("version") }
    .substringAfter('"')
    .substringBefore('"')

val workspaceVersionCode: Int = workspaceVersion.split(".").let { parts ->
    parts[0].toInt() * 10000 + parts[1].toInt() * 100 + parts[2].toInt()
}

android {
    namespace = "com.buhuipao.tunnelproxy"
    compileSdk = 35
    ndkVersion = "27.2.12479018"

    defaultConfig {
        applicationId = "com.buhuipao.tunnelproxy"
        minSdk = 26
        targetSdk = 35
        versionCode = workspaceVersionCode
        versionName = workspaceVersion

        ndk {
            abiFilters += listOf("arm64-v8a")
        }
    }

    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                storeFile = releaseStoreFile
                storePassword = keystoreProperties.getProperty("storePassword")
                keyAlias = keystoreProperties.getProperty("keyAlias")
                keyPassword = keystoreProperties.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            // Never the debug key. It is the keystore every Android SDK ships
            // with, so anyone can produce an APK that upgrades over one signed
            // with it. A missing release key stops the build instead.
            signingConfig = if (hasReleaseSigning) signingConfigs.getByName("release") else null
            isMinifyEnabled = false
        }
    }

    buildFeatures {
        buildConfig = true
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    externalNativeBuild {
        ndkBuild {
            path = file("src/main/jni/Android.mk")
        }
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    implementation("androidx.core:core:1.13.1")
    implementation("com.journeyapps:zxing-android-embedded:4.3.0")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}

// Configuring the release build type without a key would otherwise produce an
// unsigned APK that the release pipeline happily picks up and publishes.
gradle.taskGraph.whenReady {
    if (hasReleaseSigning) {
        return@whenReady
    }
    val releaseBuild = allTasks.any { task ->
        (task.name.startsWith("assemble") || task.name.startsWith("bundle")) &&
            task.name.contains("Release")
    }
    if (releaseBuild) {
        throw GradleException(
            "Release signing is not configured. Create apps/android-proxy/keystore.properties " +
                "with storeFile, storePassword, keyAlias and keyPassword pointing at the " +
                "Lantunnel release keystore.",
        )
    }
}
