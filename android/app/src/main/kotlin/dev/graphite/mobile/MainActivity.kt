package dev.graphite.mobile

import android.app.Activity
import android.os.Bundle
import android.util.Log
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View

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

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // El .so se carga antes de tocar ningún símbolo nativo.
        System.loadLibrary("graphite_android")

        surfaceView = SurfaceView(this).apply {
            // El motor pinta opaco y sin blending; pedir un formato con alfa
            // solo haría que el compositor mezcle contra el fondo por nada.
            holder.setFormat(PixelFormat.RGBX_8888)
        }
        setContentView(surfaceView)
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
        renderThread = RenderThread().also {
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
    private class RenderThread : Thread("graphite-render") {
        @Volatile
        var running = true

        override fun run() {
            val nsPorFrame = 1_000_000_000L / 60
            var siguiente = System.nanoTime()
            var n = 0L

            while (running) {
                val estado = nativeFrame()
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
external fun nativeSurfaceSize(width: Int, height: Int)
external fun nativeFrame(): String