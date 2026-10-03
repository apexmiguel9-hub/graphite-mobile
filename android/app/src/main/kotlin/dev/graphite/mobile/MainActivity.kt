package dev.graphite.mobile

import android.app.Activity
import android.graphics.PixelFormat
import android.os.Bundle
import android.util.Log
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.widget.FrameLayout

/**
 * Activity mínima: una `SurfaceView` a pantalla completa y el motor pintando
 * dentro.
 *
 * # Lo único que importa aquí
 *
 * [SurfaceHolder.Callback.surfaceChanged] **no** es un `= Unit`.
 *
 * El puerto anterior implementaba `surfaceCreated` (que en Android se dispara
 * *antes* del layout, así que la vista todavía mide 0×0) y `surfaceChanged` como
 * una función vacía. Como el puente solo llamaba a `wgpu::Surface::configure` una
 * vez, al arrancar, con el tamaño congelado en ese instante, la superficie se
 * quedaba con una extensión que no era la del `ANativeWindow`. El compositor
 * recortaba el frame y en la pantalla aparecía un triángulo negro en mitad.
 *
 * Aquí el tamaño se notifica las veces que haga falta, y el puente reconfigura
 * la superficie y el viewport del motor cada vez que cambia.
 *
 * # Por qué todavía no hay UI
 *
 * Falta la capa de UI (el frontend de Graphite es una app Svelte, y en Android
 * se neumará en un WebView). Añadirla antes de tener el render path limpio solo
 * añadiría variables: si se ve algo raro, no se sabría si es el motor o la UI.
 * Primero: que el lienzo dibuje entero. Después: UI encima.
 */
class MainActivity : Activity() {

    private lateinit var surfaceView: SurfaceView

    /** Hilo de render. Vive entre `surfaceCreated` y `surfaceDestroyed`. */
    private var renderThread: RenderThread? = null

    /** La UI. El frontend de Graphite dentro de un WebView. */
    private lateinit var webUI: WebUI

    /** El tamaño que se le pasó por última vez a la superficie. */
    @Volatile
    private var lastSize: Pair<Int, Int> = 0 to 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // El .so se carga antes de tocar ningún símbolo nativo.
        System.loadLibrary("graphite_android")

        surfaceView = SurfaceView(this).apply {
            // El motor pinta opaco y sin blending; pedir un formato con alfa
            // solo haría que el compositor mezcle contra el fondo por nada.
            holder.setFormat(PixelFormat.RGBX_8888)
        }

        webUI = WebUI()

        // Dos capas: la SurfaceView del motor al fondo, y el WebView de la UI
        // encima.
        //
        // OJO con esto: `SurfaceView` perfora un agujero en la jerarquía de
        // vistas —es una capa de hardware aparte, no un `View` normal—, así que
        // un WebView ENCIMA no se pinta de forma fiable sin `setZOrderMediaOverlay`
        // o `setZOrderOnTop`. Con el SurfaceView por defecto (detrás de la
        // ventana), el WebView sí se ve, porque es la ventana la que se compone
        // encima del SurfaceView. Por eso el motor va detrás y no delante: es lo
        // que evita el problema, en vez de pelearse con él.
        val root = FrameLayout(this).apply {
            addView(
                surfaceView,
                FrameLayout.LayoutParams(
                    FrameLayout.LayoutParams.MATCH_PARENT,
                    FrameLayout.LayoutParams.MATCH_PARENT,
                ),
            )
        }

        root.addView(
            webUI.create(this),
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT,
            ),
        )

        setContentView(root)
    }

    /**
     * Saca de la cola de Rust lo que haya pendiente y lo entrega al frontend.
     * Seguro desde cualquier hilo.
     *
     * Lo llama el hilo de render además de [WebUI.GraphiteNative.onMessage], y
     * hace falta por los dos motivos:
     *
     * - El mensaje de entrada solo se responde con el mensaje de salida que
     *   genera, pero el grafo de nodos produce resultados **después**, cuando ya
     *   no hay ninguna llamada de Java que los recoja.
     * - El frontend pide el resto por su cuenta: `initAfterFrontendReady` y los
     *   layouts salen por el hilo de render, no por el de JavaScript.
     */
    private fun bombear() {
        webUI.bombear()
    }

    override fun onResume() {
        super.onResume()
        surfaceView.holder.addCallback(callback)
    }

    override fun onPause() {
        surfaceView.holder.removeCallback(callback)
        stopRendering()
        super.onPause()
    }

    /**
     * Los tres callbacks se implementan. `surfaceChanged` es el que faltaba, y es
     * el que arregla el bug.
     */
    private val callback = object : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {
            // OJO: aquí la vista AÚN NO está medida. `surfaceView.width` es 0.
            // Por eso el arranque del motor NO recibe el tamaño: lo recibe
            // `surfaceChanged`, un instante después, con el bueno.
            val report = nativeBoot(holder.surface)
            Log.i(TAG, "arranque:\n$report")
        }

        override fun surfaceChanged(h: SurfaceHolder, format: Int, w: Int, ht: Int) {
            // Lo que reconfigura la superficie de wgpu y el viewport del motor.
            // Se llama en el giro, al abrir el teclado, en split screen y en
            // cualquier cambio de barras del sistema.
            Log.i(TAG, "surfaceChanged -> ${w}x$ht (formato $format)")
            nativeSurfaceSize(w, ht)
            lastSize = w to ht

            // El hilo arranca con el tamaño ya conocido.
            if (renderThread == null) startRendering()
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            // El `Surface` deja de ser válido: el hilo tiene que haber parado
            // ANTES de que esto devuelva, o el motor escribe en una superficie
            // liberada.
            stopRendering()
        }
    }

    private fun startRendering() {
        if (renderThread != null) return
        renderThread = RenderThread({ bombear() }).also {
            it.start()
            Log.i(TAG, "hilo de render arrancado")
        }
    }

    private fun stopRendering() {
        renderThread?.let {
            it.running = false
            try {
                it.join(2000)
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
            }
            Log.i(TAG, "hilo de render parado")
        }
        renderThread = null
    }

    /**
     * Bucle de render.
     *
     * Va a ritmo de vsync en vez de tan rápido como pueda: `PresentMode::Fifo` ya
     * limita a la tasa del display, así que renderizar más rápido solo gasta
     * batería y calor.
     */
    private class RenderThread(
        /**
         * Vacía la cola de salida del motor hacia el frontend.
         *
         * Va como parámetro y no tocando `webUI` desde aquí porque esta clase no
         * es `inner` y así no necesita ver la Activity: el hilo de render solo
         * necesita una cosa que hacer por frame.
         */
        private val bombear: () -> Unit,
    ) : Thread("graphite-render") {
        @Volatile
        var running = true

        override fun run() {
            val nsPorFrame = 1_000_000_000L / 60
            var siguiente = System.nanoTime()
            var n = 0L

            while (running) {
                val estado = nativeFrame()
                // El motor produce mensajes por su cuenta —el grafo de nodos
                // termina, llegan layouts, el frontend pide redibujar— y no hay
                // ninguna llamada de Java que los recoja. El hilo de render es
                // el único sitio donde se pueden coger sin quedárselos en la cola
                // para siempre.
                try {
                    bombear()
                } catch (e: Exception) {
                    Log.w(TAG, "fallo al bombear al frontend: ${e.message}")
                }
                n++
                // Los primeros frames y luego uno de cada 120: en logcat hay que
                // poder ver el arranque sin llenarlo de ruido.
                if (n <= 5 || n % 120 == 0L) Log.i(TAG, "frame #$n $estado")

                siguiente += nsPorFrame
                val dormir = siguiente - System.nanoTime()
                if (dormir > 0) {
                    try {
                        sleep(dormir / 1_000_000, (dormir % 1_000_000).toInt())
                    } catch (e: InterruptedException) {
                        Thread.currentThread().interrupt()
                        break
                    }
                } else {
                    // Se ha ido el retraso (GC, o el motor tardó): resincroniza en
                    // vez de acumular deuda y acelerar hasta recuperar, que es lo
                    // que pasa si no se hace.
                    siguiente = System.nanoTime()
                }
            }
            Log.i(TAG, "hilo de render terminado tras $n frames")
        }
    }

    companion object {
        const val TAG = "GRAPHITE"
    }
}

// Funciones nativas. El nombre del símbolo JNI sale de:
//   paquete + fichero + función -> Java_<paquete>_MainActivityKt_<función>
// por eso los `external` van a nivel superior y no dentro de la clase: si
// estuvieran dentro, el símbolo sería `..._MainActivity_nativeBoot`.
//
// Y NO pueden ser `private`: Kotlin manglea los nombres de las funciones
// `internal` (y añade el nombre del módulo en ciertas compilaciones), y el
// resultado es un `UnsatisfiedLinkError` en tiempo de ejecución, no un error de
// compilación. El mismo nombre tiene que estar en el `#[export_name]` de Rust.
external fun nativeBoot(surface: android.view.Surface): String

/** La ventana cambio de tamano. Reconfigura superficie y viewport. */
external fun nativeSurfaceSize(width: Int, height: Int)

/** Un frame. Devuelve el estado en texto, para el log. */
external fun nativeFrame(): String

/**
 * Saca UN mensaje de la cola de salida del motor, en base64, o `null`.
 *
 * MEDIDO: el wasm **tira el valor de retorno** de `sendNativeMessage`
 * (`frontend/wrapper/src/native_communication.rs`, `send_message_to_cef`, que
 * llama a la función y no lee el `call1`). La respuesta tiene que entrar por
 * `window.receiveNativeMessage`, y en el escritorio la llama CEF. Aquí no hay
 * CEF, así que Kotlin la inyecta con `evaluateJavascript`.
 *
 * Por eso el puente ya no devuelve la respuesta en `nativeMessage`: la encola,
 * y esta función la va sacando. Con un contador en el movil se vio el tamaño de
 * la respuesta (328 KB) mientras el contador de `receiveNativeMessage` se
 * quedaba en **cero**: el dato salía de Rust y se perdía antes de llegar al
 * frontend, sin un solo error en el log.
 */
external fun nativeDrain(): String?

// ------------------------------------------------------------------
// LAS CINCO FUNCIONES NATIVAS ESTAN AQUI, Y ESTO NO ES COSA SUELTA.
//
// MEDIDO en el movil, con el frontend ya cargando y llamando al puente:
//
//   java.lang.UnsatisfiedLinkError: No implementation found for
//     dev.graphite.mobile.WebUIKt.nativeMessage(java.lang.String)
//   (tried Java_dev_graphite_mobile_WebUIKt_nativeMessage)
//
// El simbolo JNI se compone de PAQUETE + FICHERO + FUNCION:
//
//   Java_dev_graphite_mobile_<FICHERO>Kt_<funcion>
//
// Las dos funciones de la UI estaban declaradas en `WebUI.kt`, asi que Kotlin
// buscaba el simbolo con `WebUIKt`, y Rust lo exportaba con `MainActivityKt`.
//
// Lo peor no es el fallo: es que el CI daba VERDE. El check miraba el nombre de
// las funciones pero daba por hecho el `MainActivityKt` de ahi. Las tres
// primeras estaban en `MainActivity.kt` y coincidian; las dos nuevas no, y
// nadie se dio cuenta hasta que el movil las llamo.
//
// Que las cinco esten juntas, y el check derive el nombre del fichero de verdad
// (`android/scripts/check-jni-symbols.py`), hace que esto no vuelva a pasar por
// añadir una función en el fichero que toque.
//
// Y NO pueden ser `private`: Kotlin manglea los nombres de las funciones
// `internal`, y el resultado es un `UnsatisfiedLinkError` en tiempo de ejecucion,
// no un error de compilacion.
// ------------------------------------------------------------------

/** wasm -> nativo: un mensaje del frontend en base64. Base64, o null. */
external fun nativeMessage(base64: String): String?

/**
 * El frontend ha terminado de arrancar. Devuelve base64 con lo que el motor
 * conteste, o null.
 *
 * El tipo tiene que COINCIDIR con el `jstring` que devuelve el `#[export_name]`
 * de Rust, y no con un `Unit`.
 */
external fun nativeInitialized(width: Int, height: Int): String?
