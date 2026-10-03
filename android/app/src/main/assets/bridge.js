// Puente entre el frontend de Graphite y el nativo.
//
// ------------------------------------------------------------------
// POR QUÉ ESTE FICHERO
//
// El wasm del frontend (modo `native`) se comunica con el proceso nativo por
// TRES globales de `window`. Los nombres los elige el código de upstream, en
// `frontend/wrapper/src/native_communication.rs`:
//
//   window.sendNativeMessage(ArrayBuffer)              -> lo busca con Reflect::get
//   window.receiveNativeMessageData = <ArrayBuffer>    -> lo pone el nativo
//   window.receiveNativeMessage(<ArrayBuffer>)         -> el wasm lo exporta
//   window.initializeNativeCommunication()             -> el wasm lo llama
//
// En el escritorio los define CEF (`desktop/ui/src/internal/
// render_process_handler.rs`). En un WebView NO hay nadie que los defina: el
// WebView es un Chromium sin la capa de CEF encima.
//
// Así que este fichero los define, y los conecta con el objeto `GraphiteNative`
// que expone la app por `@JavascriptInterface`. Son las dos piezas que faltaban.
//
// ------------------------------------------------------------------
// POR QUÉ BASE64 Y NO EL BUFFER ENTERO
//
// Por la interfaz de JavaScript no se puede pasar un ArrayBuffer de forma
// barata: hay que serializarlo a string. Base64 es lo más rápido que se puede
// hacer desde JS sin añadir una biblioteca.
//
// Ojo: `btoa` falla con bytes >= 0x80, que son TODOS los que no sean ASCII.
// Por eso el bucle con `& 0xff` y `String.fromCharCode`. El `String.fromCharCode`
// con la lista entera a la vez es lo que hace que no reviente el stack con
// mensajes grandes.

(function () {
	"use strict";

	// ------------------------------------------------------------------
	// Base64
	// ------------------------------------------------------------------
	function bufferToBase64(buffer) {
		const bytes = new Uint8Array(buffer);
		const trozos = [];
		// Troceado: llamar a String.fromCharCode con cientos de miles de
		// argumentos a la vez revienta el stack.
		const TROZO = 0x8000;
		for (let i = 0; i < bytes.length; i += TROZO) {
			trozos.push(
				String.fromCharCode.apply(null, Array.prototype.slice.call(bytes.subarray(i, i + TROZO))),
			);
		}
		return window.btoa(trozos.join(""));
	}

	function base64ToBuffer(b64) {
		const bin = window.atob(b64);
		const bytes = new Uint8Array(bin.length);
		for (let i = 0; i < bin.length; i++) {
			bytes[i] = bin.charCodeAt(i);
		}
		return bytes.buffer;
	}

	// ------------------------------------------------------------------
	// Los tres globales
	// ------------------------------------------------------------------

	/**
	 * wasm -> nativo.
	 *
	 * Devuelve `null` cuando el nativo no tiene respuesta. OJO: `null` es un
	 * valor válido, no un error, y hay que distinguirlo de "" —una respuesta
	 * vacía sería un mensaje de Graphite que no existe.
	 */
	window.sendNativeMessage = function (buffer) {
		try {
			return window.GraphiteNative.onMessage(bufferToBase64(buffer));
		} catch (e) {
			// Un throw aquí se traga el mensaje en silencio, porque quien llama
			// está dentro del wasm y no lo va a propagar. Se loguea en la
			// consola del WebView, que es como se verá en `adb logcat` con
			// el tag chromium.
			console.error("[graphite] sendNativeMessage falló:", e);
			return null;
		}
	};

	/**
	 * El frontend acaba de arrancar y pide conexión.
	 *
	 * Equivale al `on_context_created` de CEF. Se pasa el tamaño de la ventana
	 * porque el frontend lo necesita para maquetar los paneles, y lo lee de
	 * `window.innerWidth/Height` en vez de hardcodearlo.
	 */
	window.initializeNativeCommunication = function () {
		try {
			window.GraphiteNative.onInitialized(Math.round(window.innerWidth), Math.round(window.innerHeight));
		} catch (e) {
			console.error("[graphite] initializeNativeCommunication falló:", e);
		}
	};

	// ------------------------------------------------------------------
	// nativo -> wasm
	//
	// `receiveNativeMessageData` + `receiveNativeMessage` es el mismo dance que
	// hace el nativo en el escritorio con `execute_java_script`. Se mantiene el
	// nombre `...Data` porque es lo que el código de upstream busca; si se
	// renombra aquí, el glue deja de encontrarlo y falla en el móvil.
	// ------------------------------------------------------------------
	window.graphiteDeliverToWeb = function (base64) {
		try {
			window.receiveNativeMessageData = base64ToBuffer(base64);
			if (typeof window.receiveNativeMessage === "function") {
				window.receiveNativeMessage(window.receiveNativeMessageData);
			} else {
				// El wasm todavía no está listo. No es un error: el frontend
				// llama a `initWasm()` en su `onMount`, y el nativo puede
				// tener respuesta antes. Se avisa una vez, no en cada mensaje.
				if (!window.__graphiteWasmReady) {
					window.__graphiteWasmReady = false;
					console.warn("[graphite] llegó un mensaje antes de que el wasm estuviera listo; se descarta");
				}
			}
		} catch (e) {
			console.error("[graphite] entrega al web falló:", e);
		}
	};

	console.log("[graphite] bridge.js cargado");
})();
