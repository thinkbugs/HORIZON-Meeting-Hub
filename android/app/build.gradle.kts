plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.horizon.castsender"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.horizon.castsender"
        minSdk = 26
        targetSdk = 35
        versionCode = 9
        versionName = "0.5.1"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // Sideloaded builds use the debug key; the project has no release
            // keystore, and an unsigned APK cannot be installed.
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

    testOptions {
        unitTests.isReturnDefaultValues = true
    }
}

dependencies {
    // Wire/WsClient/Discovery are plain JVM code so the protocol can be tested
    // without a device. org.json is a stub inside android.jar, hence the real
    // implementation on the test classpath only.
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}

tasks.withType<Test>().configureEach {
    // ReceiverLiveTest and HubMediaLiveTest read these; without them they skip.
    listOf("horizonHost", "horizonPort", "horizonToken", "horizonH264", "horizonSize", "horizonHub").forEach { key ->
        (project.findProperty(key) as String?)?.let { systemProperty(key, it) }
    }
}
