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
// UNA DEPENDENCIA: `androidx.webkit`. Ni Compose ni AndroidX, a propósito.
//
// `WebViewAssetLoader` esta en `androidx.webkit`, no en el framework. MEDIDO:
// en el `android.jar` de API 36 no hay ninguna clase `AssetLoader`. Y hace
// falta:
//
//   Access to script at 'file:///android_asset/web/bundle.js' from origin 'null'
//   has been blocked by CORS policy
//
// `bundle.js` es un modulo (`type="module"`, por el `import.meta` con el que
// localiza su `.wasm`), y un modulo ES una peticion. Con `file:` el origen es
// `null` y no se permite. `WebViewAssetLoader` sirve los assets por
// `https://appassets.androidplatform.net`, que es un origen real.
//
// Sigue sin haber Compose ni el resto de AndroidX: si la pantalla sale rara, hay
// que poder decir si es el motor, el WebView, o la composicion de las dos capas,
// y eso es mucho mas facil cuando no hay un framework de UI en medio.
// ------------------------------------------------------------------
dependencies {
    implementation("androidx.webkit:webkit:1.12.1")

    // ------------------------------------------------------------------
    // `androidx.core`, y SOLO por los insets del sistema.
    //
    // Se declara explícitamente aunque `androidx.webkit` ya lo traiga de forma
    // transitiva, porque se usa directamente. Pedirle a una dependencia
    // transitiva algo que se usa en código propio es la forma de que un día
    // falle sin que se haya tocado nada: cambia `webkit` y se va.
    //
    // Es la única excepción a lo de no meter AndroidX de UI, y no mete ningún
    // framework de UI: `ViewCompat` y `WindowInsetsCompat` son envolturas de
    // la API del framework. Sirven para lo mismo en API 26 y en API 36, que con
    // `View.setOnApplyWindowInsetsListener` a pelo habría que bifurcar a mano.
    //
    // MEDIDO: con `targetSdk = 36` la ventana va de edge-to-edge SIEMPRE, desde
    // Android 15, y no hay forma de desactivarlo. La barra de título de Graphite
    // se dibujaba debajo de las notificaciones y el panel derecho se cortaba con
    // la barra de navegación. Ver `MainActivity.aplicarInsets`.
    // ------------------------------------------------------------------
    implementation("androidx.core:core-ktx:1.13.1")
}