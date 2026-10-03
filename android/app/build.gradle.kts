plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.graphite.mobile"
    compileSdk = 36
    buildToolsVersion = "36.0.0"
    // La misma versión que usa el workflow que compila el .so. Si divergen, el
    // .so se compila con un clang y se empaqueta con headers de otro.
    ndkVersion = "28.2.13676358"

    defaultConfig {
        // Distinto del `dev.graphite.app` del APK anterior a propósito: así los
        // dos APKs conviven en el mismo móvil y se pueden comparar sin
        // desinstalar nada.
        applicationId = "dev.graphite.mobile"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"

        // Solo arm64-v8a. El motor son 58 MB por ABI; meter cuatro armazeis cuatro
        // APK, y el móvil de prueba es arm64.
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }

    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    // La UI (`assets/web/`) la baja el workflow, no está en el repo: son 6 MB de
    // bundle con hash, que cambian en cada build de upstream. En el repo solo
    // están `index.html` y `bridge.js`, que son de este repo y sí se versionan.
    sourceSets["main"].assets.srcDirs("src/main/assets")
}

// ------------------------------------------------------------------
// SIN DEPENDENCIAS. Sigue siendo una decisión.
//
// La UI es el frontend de Graphite dentro de un WebView, y `android.webkit` es
// del framework, no de AndroidX. Así que sigue sin make needing nada.
//
// Ni Compose ni AndroidX a propósito: si la pantalla sale rara, hay que poder
// decir si es el motor, el WebView, o la composición de las dos capas. Eso es
// mucho más fácil cuando no hay un framework de UI en medio. Cuando haya que
// portar algo a nativo, se añade entonces.
// ------------------------------------------------------------------
dependencies {
}