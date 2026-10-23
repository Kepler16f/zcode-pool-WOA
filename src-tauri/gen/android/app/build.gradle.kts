import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

android {
    compileSdk = 36
    namespace = "com.zpool.app"
    // release 签名由环境变量注入（CI 用 cache 持久化的 keystore 生成；公开仓库不放密钥、不设 secrets）。
    // 没给 ZPOOL_KEYSTORE 时 release 产物是 unsigned 的，构建照常成功，只是装不上——debug 不受影响。
    signingConfigs {
        val ksPath = System.getenv("ZPOOL_KEYSTORE")
        if (!ksPath.isNullOrBlank() && file(ksPath).exists()) {
            create("zpool") {
                storeFile = file(ksPath)
                storePassword = System.getenv("ZPOOL_KEYSTORE_PASSWORD") ?: "zpool2026"
                keyAlias = System.getenv("ZPOOL_KEY_ALIAS") ?: "zpool"
                keyPassword = System.getenv("ZPOOL_KEY_PASSWORD") ?: storePassword
            }
        }
    }
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.zpool.app"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = "Android_" + tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            // 反代/管理台都是 http://（局域网 + 回环），release 也必须放行明文流量，
            // 否则 Android 9+ 直接掐断 WebView 到 8899 的 fetch。
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            // 体积主要靠 release Rust（strip+lto）省下来；minify 关掉避免 proguard
            // 误删 wry/JNI 反射类引发运行时崩溃，省的那点 Java 体积不值这个风险。
            isMinifyEnabled = false
            signingConfigs.findByName("zpool")?.let { signingConfig = it }
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")