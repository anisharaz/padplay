plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.padplay.display"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.padplay.display"
        // API 29 is the floor for LocalServerSocket usage as written; the
        // target tablet is API 34.
        minSdk = 29
        targetSdk = 34
        versionCode = 10
        versionName = "0.5.1"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Signed with the debug key so `adb install` works without
            // provisioning a keystore. This app is sideloaded, never published.
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    lint {
        // Lint suggests merging mipmap-anydpi-v26 into a bare mipmap-anydpi
        // since minSdk 29 already implies v26+. That folder name doesn't
        // actually resolve for this AGP/AAPT2 version -- verified by trying
        // it, not by assumption -- so keep the working -v26 qualifier and
        // suppress the suggestion rather than ship a build that only works
        // by accident.
        disable += "ObsoleteSdkInt"
    }
}
