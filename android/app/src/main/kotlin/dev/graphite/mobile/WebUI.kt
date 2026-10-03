package dev.graphite.mobile

import android.annotation.SuppressLint
import android.content.Context
import android.util.Log
import android.webkit.ConsoleMessage
import android.webkit.JavascriptInterface
import android.webkit.WebChromeClient
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient

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
    private var cargado = false

    /** Se llama con cada mensaje que el nativo devuelve hacia el frontend. */
    var onDeliverToWeb: ((String) -> Unit)? = null

    /** DiAGNóstico a la consola del WebView, visible en logcat con `chromium`. */
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

        webViewClient = object : WebViewClient() {
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
                // El shim se inyecta ANTES de que corra el wasm, no despues. Si
                // se inyectara despues, el wasm ya habria intentado llamar a
                // `sendNativeMessage` y no lo habria encontrado.
                inyectarShim(view)
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

        // El shim va en la carga de la página, no después: el wasm se ejecuta
        // durante la carga y necesita los globales ya definidos.
        loadUrl("file:///android_asset/web/index.html")
    }

    /**
     * Inyecta [bridge.js] en la página.
     *
     * Se lee de los assets y se evalúa en la página, porque el fichero tiene
     * que estar disponible para el `fetch` inicial pero no debe estar dentro
     * del bundle de vite.
     */
    private fun inyectarShim(view: WebView) {
        // MEDIDO: cargar el shim con `document.createElement('script')` y un
        // `src` a `file://` da dos fallos, y el log los enseña los dos:
        //
        //   1. net::ERR_FAILED en bridge.js
        //   2. Access to script at 'file://...' from origin 'null' has been
        //      blocked by CORS policy: ... only supported for protocol schemes:
        //      chrome, data, http, https
        //
        // El segundo es el importante: una pagina `file://` tiene origen `null`,
        // y `fetch`/`import` de otro fichero de disco son peticiones
        // cross-origin contra origen nulo, que el navegador bloquea.
        //
        // `evaluateJavascript` no hace peticion ninguna: el codigo viaja dentro
        // del propio JavaScript evaluado. Por eso el shim va EMBEBIDO como
        // constante y no se carga como fichero.
        view.evaluateJavascript(SHIM_JS, null)
        Log.i(TAG, "shim inyectado (embebido, sin fetch)")
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

// Funciones nativas. Ver el comentario del final de MainActivity.kt sobre por
// que esto NO puede ser `private`.
external fun nativeMessage(base64: String): String?
// Devuelve `String?`: el nativo contesta con lo que el motor mande al frontend.
// El tipo tiene que COINCIDIR con el `jstring` que devuelve el `#[export_name]`
// de Rust, y no con un `Unit`.
external fun nativeInitialized(width: Int, height: Int): String?