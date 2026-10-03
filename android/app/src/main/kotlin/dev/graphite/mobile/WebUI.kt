package dev.graphite.mobile

import android.annotation.SuppressLint
import android.content.Context
import android.util.Log
import android.webkit.ConsoleMessage
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceResponse
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.WebViewAssetLoader
import androidx.webkit.WebViewAssetLoader.AssetsPathHandler

/**
 * La UI: el frontend de Graphite (Svelte) dentro de un WebView.
 *
 * # Por qué un WebView y no una traducción a Kotlin
 *
 * El frontend de Graphite son 15.101 líneas de Svelte. Reescribirlas en Compose
 * son ~38 lotes, y lo más caro —los overlays del lienzo, los gestos táctiles, el
 * CSS— no sale gratis en ninguna traducción.
 *
 * Pero el frontend **ya sabe** hablar con un motor nativo: es lo que hace con
 * CEF en el escritorio. Lo que faltaba era el transporte, y aquí se suprime
 * CEF por WebView.
 *
 * # El contrato
 *
 * El wasm del frontend se comunica por tres globales de `window`
 * (`frontend/wrapper/src/native_communication.rs`). El WebView es un Chromium
 * pelado, sin CEF encima, así que nadie los define: los define
 * `assets/web/bridge.js`, y este objeto [GraphiteNative] es el otro extremo.
 *
 * ```
 *   wasm --sendNativeMessage(base64)-->  bridge.js
 *        --@JavascriptInterface------->  GraphiteNative.onMessage
 *        --nativeMessage (JNI)--------->  Rust: dispatch -> respuesta
 *        <----------- base64 (JNI) ------
 *        <------- evaluateJavascript --  bridge.js: receiveNativeMessage
 *   wasm
 * ```
 *
 * # Por qué base64
 *
 * Por la interfaz de JavaScript no se puede pasar un `ArrayBuffer` de forma
 * barata, hay que serializarlo a string. Y el camino inverso tampoco: `WebView`
 * no tiene una API para inyectar un buffer. Base64 es lo más simple que funciona
 * en los dos sentidos.
 */
@SuppressLint("SetJavaScriptEnabled")
class WebUI {
    private var webView: WebView? = null

    /**
     * El asset loader. MEDIDO: `WebView` NO tiene `setAssetLoader` — leidas sus
     * metodos del `android.jar` de API 36, no existe. Todo el soporte de
     * origins pasa por `shouldInterceptRequest`, asi que el loader vive aqui y
     * el `WebViewClient` se lo consulta.
     */
    private lateinit var loader: WebViewAssetLoader

    private var cargado = false

    /** Se llama con cada mensaje que el nativo devuelve hacia el frontend. */
    var onDeliverToWeb: ((String) -> Unit)? = null

    /** Estado de la carga, para el log. */
    private var ultimoEstado = ""

    /**
     * Crea el WebView.
     *
     * El `context` es la propia Activity: un WebView sin Activity no tiene
     * ventana y no carga nada. `WebView(null)` compila pero no funciona, que es
     * peor que un error.
     */
    fun create(context: Context): WebView = WebView(context).apply {
        webView = this

        settings.apply {
            javaScriptEnabled = true
            // El frontend es una SPA con rutas como /demo-artwork/foo.graphite.
            // Sin esto, al recargar pierde el estado y hay que gestionarlo a mano.
            domStorageEnabled = true
            // El frontend se sirve desde assets, no de la red. Sin esto,
            // cualquier fetch() a un origen externo se bloquea; con esto, se
            // puede cargar el arte de demo desde los propios assets.
            allowFileAccess = true
            allowContentAccess = true
            // No hay barra de direcciones ni menús: es una app, no un navegador.
            // Nada del frontend depende de esto.
            setSupportZoom(false)
            builtInZoomControls = false
            displayZoomControls = false
            // Sin caché de red: los assets son locales y un caché stale da
            // bugs de "he cambiado el bridge.js y sigue el viejo".
            cacheMode = android.webkit.WebSettings.LOAD_NO_CACHE
            mediaPlaybackRequiresUserGesture = false
        }

        // ------------------------------------------------------------------
        // EL ASSET LOADER, ANTES DEL CLIENTE.
        //
        // MEDIDO: `WebViewAssetLoader` esta en `androidx.webkit`, NO en el
        // framework — comprobado en el `android.jar` de API 36, donde no hay
        // ninguna clase `AssetLoader`. De ahi la dependencia.
        //
        // Va antes del `WebViewClient` porque el cliente lo consulta en
        // `shouldInterceptRequest`, y ese método no puede encontrar un
        // `lateinit` sin inicializar.
        // ------------------------------------------------------------------
        loader = WebViewAssetLoader.Builder()
            // `setDomain` espera SOLO el host, sin esquema. Ponerlo con
            // `https://` ahi no funciona y el error no lo explica.
            .setDomain(HOST)
            // `/assets/` -> `assets/web/` del APK. El prefijo del handler ES la
            // carpeta: `/assets/glue.wasm` se sirve desde `assets/web/glue.wasm`.
            //
            // MEDIDO: con el handler en `/assets/` -> `assets/` (la raiz), el wasm
            // no se encontraba, porque el bundle lo pide por ruta ABSOLUTA y
            // estaba un nivel mas abajo:
            //
            //   error al cargar -1 en .../assets/glue.wasm
            //   Failed to execute 'compile' on 'WebAssembly': HTTP status code
            //   is not ok
            //
            // Con el prefijo apuntando a la carpeta de la UI, `/assets/...` cae
            // justo dentro. Y la pagina se carga de `/assets/index.html`.
            .addPathHandler(PREFIJO_WEB, AssetsPathHandler(context))
            .build()

        webViewClient = object : WebViewClient() {
            /**
             * Sirve los assets por un ORIGEN REAL.
             *
             * MEDIDO: `WebView` no tiene `setAssetLoader` — leidas sus metodos
             * del `android.jar` de API 36, no existe. Todo el soporte de origins
             * pasa por aqui, que es donde el loader decide si sirve algo o
             * deja pasar la peticion.
             *
             * Sin esto, el bundle queda bloqueado:
             *
             *   Access to script at '.../bundle.js' from origin 'null' has been
             *   blocked by CORS policy
             *
             * porque un modulo ES una peticion y con `file:` el origen es nulo.
             */
            override fun shouldInterceptRequest(
                view: WebView,
                peticion: WebResourceRequest,
            ): WebResourceResponse? = loader.shouldInterceptRequest(peticion.url)

            /**
             * Aqui, y no en `onPageFinished`.
             *
             * MEDIDO: inyectando el shim en `onPageFinished`, el frontend
             * arrancaba y el wasm no encontraba `sendNativeMessage`:
             *
             *   Uncaught TypeError: window.sendNativeMessage is not a function
             *
             * El motivo es el orden: un `<script type="module">` se evalua como
             * modulo, y el modulo se evalua ANTES de que la pagina termine de
             * cargar. `onPageFinished` va despues. El shim tiene que existir
             * cuando el wasm llame a `Reflect::get(global, "sendNativeMessage")`,
             * y ese momento es durante la evaluacion del modulo.
             *
             * `onPageStarted` salta con la pagina todavia vacia, que es justo lo
             * que hace falta: el shim se instala y luego se evalua el bundle.
             *
             * Con `WebViewAssetLoader` esto es menos delicado, porque el modulo
             * se descarga de un origen real y tarda unas cuantas iteraciones del
             * event loop. Pero el orden sigue siendo el correcto y no depende de
             * ese tiempo.
             */
            override fun onPageStarted(view: WebView, url: String, favicon: android.graphics.Bitmap?) {
                Log.i(TAG, "empieza a cargar: $url")
                inyectarShim(view)
            }
            // ------------------------------------------------------------------
            // LA FIRMA REAL, LEIDA DEL ANDROID.JAR, NO SUPUESTA.
            //
            // MEDIDO, extrayendo la clase del `android.jar` de API 36:
            //
            //   WebViewClient.onReceivedError(
            //       WebView, WebResourceRequest, WebResourceError)
            //
            // O sea que en API 23+ NO es `Int` + `String`: el codigo va en un
            // `WebResourceError` y hay que sacar `errorCode` y `description` de
            // ahi. La firma antigua de 4 parametros existe pero esta deprecada.
            //
            // Esto costaba tres iteraciones de CI por adivinar. La firma esta
            // escrita arriba, copiada del `.class`, para que no haya que
            // adivinarla otra vez.
            // ------------------------------------------------------------------
            override fun onReceivedError(
                view: WebView,
                request: WebResourceRequest,
                error: WebResourceError,
            ) {
                // Con `LOAD_NO_CACHE` y assets locales no deberia pasar, pero si
                // pasa es un fallo de carga de la UI entera y hay que verlo.
                Log.e(
                    TAG,
                    "error al cargar ${error.errorCode} (${error.description}) " +
                        "en ${request.url} isForMainFrame=${request.isForMainFrame}",
                )
            }

            override fun onPageFinished(view: WebView, url: String) {
                Log.i(TAG, "el frontend ha terminado de cargar: $url")
                // OJO: no se inyecta aqui. `onPageFinished` salta DESPUES de que los
                // modulos se hayan evaluado, y el bundle es un modulo: cuando
                // esto corre, el wasm ya ha intentado llamar a
                // `sendNativeMessage` y no lo ha encontrado.
                //
                // El shim se inyecta en `onPageStarted`, con la pagina todavia
                // vacia. Ver `onPageStarted`.
                Log.i(TAG, "la pagina ha terminado de cargar: $url")
                cargado = true
            }
        }

        /**
         * La consola del WebView.
         *
         * **Va en `WebChromeClient`, no en `WebViewClient`.** Este era el
         * motivo de "overrides nothing": el metodo no estaba en la clase que se
         * estaba heredando.
         *
         * MEDIDO, leyendo las clases del `android.jar` de API 36:
         *
         *   WebViewClient    onReceivedError(WebView, WebResourceRequest, WebResourceError)
         *   WebChromeClient  onConsoleMessage(ConsoleMessage): boolean
         *
         * Los dos son `public`, no `protected`. Los dos intentos anteriores de
         * cambiar la visibilidad eran ruido: el problema era que el metodo no
         * existia en esa clase.
         */
        webChromeClient = object : WebChromeClient() {
            override fun onConsoleMessage(msg: ConsoleMessage): Boolean {
                // El `console.log` del frontend acaba en logcat con el tag del
                // proceso del navegador, no con el nuestro. Envolverlo aqui lo
                // mete todo bajo GRAPHITE, que es donde se mira.
                val tag = msg.sourceId()?.substringAfterLast('/') ?: "web"
                val linea = "[$tag] ${msg.message()} @${msg.lineNumber()}"
                when (msg.messageLevel()) {
                    ConsoleMessage.MessageLevel.ERROR -> Log.e(TAG, linea)
                    ConsoleMessage.MessageLevel.WARNING -> Log.w(TAG, linea)
                    else -> Log.i(TAG, linea)
                }
                return true
            }
        }

        addJavascriptInterface(GraphiteNative(), "GraphiteNative")

        // ------------------------------------------------------------------
        // SERVIR POR UN ORIGEN REAL, NO POR `file://`.
        //
        // MEDIDO: con `file://`, el bundle queda bloqueado y el frontend no
        // arranca nunca.
        //
        //   Access to script at 'file:///android_asset/web/bundle.js' from
        //   origin 'null' has been blocked by CORS policy: Cross origin requests
        //   are only supported for protocol schemes: chrome, chrome-untrusted,
        //   data, http, https
        //
        // La causa NO es el atributo del script. Es que `bundle.js` se declara
        // `type="module"`, y **un módulo ES una petición**: con esquema `file:`
        // el origen es `null`, y el navegador no permite peticiones
        // cross-origin contra origen nulo. Un `<script>` clásico pasaría, pero el
        // bundle necesita ser módulo por el `import.meta` con el que localiza su
        // propio `.wasm`.
        //
        // `WebViewAssetLoader` mapea `/assets/` a los assets del APK y lo sirve
        // por `https://appassets.androidplatform.net`: `https` de verdad, con su
        // propio origen. Los módulos y el `fetch` funcionan sin tocar una línea
        // del frontend.
        //
        // El prefijo `/assets/web/` importa: el bundle pide `/assets/glue.wasm` y
        // las miniaturas por `/assets/thumbnail-*.png`, rutas ABSOLUTAS, y
        // resuelven a `https://appassets.androidplatform.net/assets/glue.wasm`.
        // El path handler mapea `/assets/` → `assets/` del APK, asi que el
        // `assets/` del APK tiene que contener lo que el bundle pide. Por eso el
        // `.wasm` y las miniaturas van en `assets/web/assets/`.
        loadUrl("$DOMINIO$PREFIJO_WEB" + "index.html")
    }

    /**
     * Instala el shim en la página.
     *
     * Va EVALUADO, no cargado como fichero: una página `file://` tiene origen
     * `null` y cualquier petición a otro fichero de disco la bloquea. Con
     * `WebViewAssetLoader` eso ya no pasa, pero `evaluateJavascript` sigue siendo
     * lo correcto: no hace peticiones y el shim está disponible antes de que
     * corra nada.
     *
     * MEDIDO: si el marcador `SHIM_PLACEHOLDER` llega intacto, lo que se evalúa
     * es JavaScript que dice `SHIM_PLACEHOLDER is not defined`. Pasa cuando
     * `build-frontend` no incrustó el shim. Se comprueba aquí para que el fallo
     * diga algo útil en vez de un ReferenceError.
     */
    private fun inyectarShim(view: WebView) {
        if (SHIM_JS.contains("SHIM_PLACEHOLDER")) {
            Log.e(TAG, "BUG: el shim NO fue incrustado; SHIM_JS es el marcador")
            Log.e(TAG, "build-frontend no se ejecutó, o se usó un WebUI.kt viejo")
            return
        }
        view.evaluateJavascript(SHIM_JS, null)
        Log.i(TAG, "shim inyectado (embebido, ${SHIM_JS.length} bytes)")
    }

    /**
     * Entrega al frontend un mensaje del nativo, en base64.
     *
     * Se llama desde el hilo nativo, **no** desde el principal: `WebView` es
     * de un solo hilo, y tocarla desde el hilo de render peta. Por eso el
     * `post`.
     */
    fun deliver(base64: String) {
        val wv = webView
        if (wv == null) {
            Log.w(TAG, "mensaje del nativo sin WebView; descartado")
            return
        }
        wv.post {
            try {
                wv.evaluateJavascript("window.graphiteDeliverToWeb('$base64');", null)
            } catch (e: Exception) {
                // Con mensajes grandes, `evaluateJavascript` puede fallar si el
                // harness está ocupado. Se avisa en vez de morir: perder un
                // frame de UI es mucho mejor que matar el proceso.
                Log.e(TAG, "no se pudo entregar al frontend: ${e.message}")
            }
        }
    }

    /** ¿Ha terminado de cargar el frontend? */
    fun isLoaded(): Boolean = cargado

    fun status(): String = if (cargado) "cargado" else "cargando ($ultimoEstado)"

    /**
     * El otro extremo del puente, visible desde JS como `window.GraphiteNative`.
     *
     * Los métodos son públicos porque `@JavascriptInterface` los busca por
     * reflexión desde el hilo del JavaScript. No se puede llamar a Kotlin con
     * modificadores de visibility rareados: se llama igual.
     */
    inner class GraphiteNative {

        /**
         * wasm -> nativo. Devuelve base64, o `null` si no hay respuesta.
         *
         * OJO: esto se llama desde el hilo del JavaScript, no desde el de
         * render. El puente Rust serializa con un `Mutex`, asi que no hay
         * carrera; y no puede haber reentrada porque la ruta de vuelta
         * (`deliver`) hace `post` al hilo principal en vez de llamar a JS
         * desde aqui.
         */
        @JavascriptInterface
        fun onMessage(base64: String): String? {
            val r = nativeMessage(base64)
            Log.i(TAG, "js -> nativo: ${base64.length} b64 entrada, ${r?.length ?: 0} b64 salida")
            return r
        }

        /** El frontend ha terminado de arrancar.
         *
         * Devuelve base64 con lo que el motor conteste, o `null`. Es la misma
         * vía que [onMessage]: el frontend ya está listo, así que en vez de
         * inventar un canal de vuelta aparte se le entrega la respuesta por el
         * mismo hilo que acaba de llamar, y el shim la encamina a
         * `receiveNativeMessage` como cualquier otro mensaje.
         */
        @JavascriptInterface
        fun onInitialized(width: Int, height: Int): String? {
            Log.i(TAG, "el frontend pide conexión: ${width}x$height")
            return nativeInitialized(width, height)
        }
    }

    companion object {
        const val TAG = "GRAPHITE"

        /**
         * Origen real para los assets. `https` de verdad, no `file:`.
         *
         * MEDIDO: `WebViewAssetLoader` esta en `androidx.webkit`, NO en el
         * framework — comprobado en el `android.jar` de API 36, donde no hay
         * ninguna clase `AssetLoader`. De ahi la dependencia.
         */
        const val DOMINIO = "https://appassets.androidplatform.net"

        /**
         * Solo el host, sin esquema. `WebViewAssetLoader.setDomain` lo espera
         * asi; con `https://` delante no funciona.
         */
        const val HOST = "appassets.androidplatform.net"

        /**
         * Prefijo del handler de assets. `/assets/` -> `assets/` del APK.
         *
         * MEDIDO: `AssetsPathHandler` solo tiene el constructor de un argumento,
         * que sirve la raiz de `assets/`. No hay forma de pedirle un
         * subdirectorio. Por eso la UI vive en la raiz: es la unica disposicion
         * en la que las rutas ABSOLUTAS del bundle caen dentro del prefijo.
         */
        const val PREFIJO_WEB = "/assets/"

        /**
         * El shim, embebido en el binario y no cargado como fichero.
         *
         * MEDIDO: cargarlo con `document.createElement('script')` y un `src` a
         * `file://` falla con CORS, porque una pagina `file://` tiene origen
         * `null` y cualquier peticion a otro fichero de disco es cross-origin
         * contra origen nulo:
         *
         *   Access to script at 'file:///android_asset/web/bridge.js' from
         *   origin 'null' has been blocked by CORS policy
         *
         * `evaluateJavascript` no hace ninguna peticion: el codigo viaja dentro
         * del propio JavaScript evaluado.
         *
         * EL CONTENIDO SE GENERA AL EMPAQUETAR. La fuente es
         * `android/app/src/main/assets/web/bridge.js` y
         * `android/scripts/embed-shim.py` lo incrusta aqui sustituyendo el
         * marcador. El `.js` se versiona aparte para poder editar 130 lineas de
         * JavaScript sin pelearse con el delimitador de un raw string de Kotlin.
         *
         * Este `companion object` es UNO solo. Habia dos: el segundo perdi
         * `SHIM_PLACEHOLDER` al borrar el primero por error, y el compilador
         * avisaba con "Conflicting declarations" sin decir de que.
         */
        private val SHIM_JS: String = """SHIM_PLACEHOLDER"""
    }
}
