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
}

// ------------------------------------------------------------------
// SIN DEPENDENCIAS. A propósito.
//
// Esta primera versión es solo el lienzo: `SurfaceView` + el motor. La capa de UI
// (el frontend Svelte de Graphite, dentro de un WebView) se añade después.
//
// No meter Compose ni AndroidX ahora es una decisión, no una omisión: si la
// pantalla sale rara, hay que poder decir si es el motor o la UI, y eso es mucho
// más fácil cuando no hay UI. Además, cada dependencia que se añade aquí es una
// variable más en un `build-apk` que tarda.
//
// Nada de esto necesita AndroidX: el tema es de plataforma
// (`Theme.Material.NoActionBar`) y el SurfaceView es del framework.
// ------------------------------------------------------------------
dependencies {
}