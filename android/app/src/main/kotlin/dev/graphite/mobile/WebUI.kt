package dev.graphite.mobile

import android.annotation.SuppressLint
import android.content.Context
import android.util.Log
import android.webkit.ConsoleMessage
import android.webkit.JavascriptInterface
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
            override fun onPageFinished(view: WebView, url: String) {
                Log.i(TAG, "el frontend ha terminado de cargar: $url")
                // El shim se inyecta ANTES de que corra el wasm, no después.
                // Si se inyectara al terminar de cargar la página, el wasm ya
                // habría intentado llamar a `sendNativeMessage` y no lo habría
                // encontrado.
                inyectarShim(view)
                cargado = true
            }

            // En API 23+ la firma lleva `request`. Sin ese parametro el
            // override no casa y sale "overrides nothing".
            //
            // MEDIDO: se probaron dos cosas que NO eran la causa, y las dos
            // dan el mismo error: ponerlos `protected` en vez de `private`, y
            // quitar el `request`. Aqui van `public`, que es como los declara
            // `WebViewClient`. La visibilidad no era el problema.
            @Deprecated("Deprecated in Java")
            @Suppress("DEPRECATION")
            override fun onReceivedError(
                view: WebView?,
                request: WebResourceRequest?,
                errorCode: Int,
                description: String?,
                failingUrl: String?,
            ) {
                // Con `LOAD_NO_CACHE` y assets locales no debería pasar, pero
                // si pasa es un fallo de carga de la UI entera y hay que verlo.
                Log.e(TAG, "error al cargar: $errorCode $description en $failingUrl")
            }

            override fun onConsoleMessage(msg: ConsoleMessage): Boolean {
                // `console.log` del frontend acaba en logcat con el tag del
                // proceso, no con el nuestro. Envolverlo aquí mete todo bajo
                // GRAPHITE, que es donde se mira.
                val tag = msg.sourceId()?.substringAfterLast('/') ?: "web"
                when (msg.messageLevel()) {
                    ConsoleMessage.MessageLevel.ERROR ->
                        Log.e(TAG, "[$tag] ${msg.message()} @${msg.lineNumber()}")
                    ConsoleMessage.MessageLevel.WARNING ->
                        Log.w(TAG, "[$tag] ${msg.message()} @${msg.lineNumber()}")
                    else -> Log.i(TAG, "[$tag] ${msg.message()} @${msg.lineNumber()}")
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
        view.evaluateJavascript(SHIM_JS, null)
        Log.i(TAG, "shim inyectado")
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
         * El shim, embebido en el binario.
         *
         * Va como constante en vez de leerse de `assets/web/bridge.js` porque
         * así no puede desincronizarse con el APK: si el fichero falta o está
         * corrupto, el APK ni siquiera compila.
         */
        private const val SHIM_JS = """
            (function(){
              var s=document.createElement('script');
              s.src='file:///android_asset/web/bridge.js';
              s.onload=function(){console.log('[graphite] shim ejecutado')};
              s.onerror=function(){console.error('[graphite] NO SE PUDO CARGAR bridge.js')};
              document.head.appendChild(s);
            })();
        """
    }
}

// Funciones nativas. Ver el comentario del final de MainActivity.kt sobre por
// que esto NO puede ser `private`.
external fun nativeMessage(base64: String): String?
// Devuelve `String?`: el nativo contesta con lo que el motor mande al frontend.
// El tipo tiene que COINCIDIR con el `jstring` que devuelve el `#[export_name]`
// de Rust, y no con un `Unit`.
external fun nativeInitialized(width: Int, height: Int): String?