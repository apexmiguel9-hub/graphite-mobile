//! Puente JNI.
//!
//! # La parte enrevesada de JNI (que ya costó un ciclo de builds)
//!
//! - `env.get_raw()` da el puntero CRUDO **sin consumir** el `env`. El binding
//!   del NDK (`NativeWindow::from_surface`) lo necesita, y además `get_raw`
//!   deja el `env` usable para seguir haciendo llamadas JNI. De ahí el
//!   `*mut JNIEnv` crudo en la firma de `Engine::boot`.
//! - `new_global_ref` es lo que permite que la superficie sobreviva a la llamada
//!   JNI. `GlobalRef` se borra en su `drop`; no existe `delete_global_ref`.
//! - Los símbolos exportados llevan sufijo de firma porque tienen parámetros, y
//!   el nombre tiene que coincidir exactamente con el `external fun` de Kotlin
//!   (`MainActivityKt` = el fichero `MainActivity.kt`, functions de nivel superior).

use std::collections::VecDeque;
use std::sync::Mutex;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;

use crate::{Engine, TAG};

/// Una sola instancia del motor para toda la app.
static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

fn string(env: &mut JNIEnv<'_>, s: String) -> jstring {
    env.new_string(s).map(|x| x.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Arranca GPU + motor + documento. **No necesita el tamaño de la ventana**, y
/// esa es la mitad del arreglo: el tamaño llega después, en
/// `nativeSurfaceSize`, y se puede volver a mandar cuantas veces haga falta.
#[export_name = "Java_dev_graphite_mobile_MainActivityKt_nativeBoot"]
pub extern "system" fn native_boot_jni<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    surface: JObject<'local>,
) -> jstring {
    init_logger();

    let global = match env.new_global_ref(surface) {
        Ok(g) => g,
        Err(e) => {
            let msg = format!("new_global_ref falló: {e:?}");
            log::error!("[{}] {}", TAG, msg);
            return string(&mut env, format!("FALLO\n{msg}"));
        }
    };

    // ------------------------------------------------------------------
    // ¿SEGUNDA VEZ? ENTONCES SOLO LA SUPERFICIE.
    //
    // MEDIDO, y esto es el crash de volver del segundo plano:
    //
    //     signal 6 (SIGABRT), en el hilo principal
    //     #13 Java_dev_graphite_mobile_MainActivityKt_nativeBoot+584
    //     #19 MainActivity$callback$1.surfaceCreated+0
    //     #24 android.view.SurfaceView.surfaceCreated
    //     #23 android.view.SurfaceView.setWindowStopped
    //
    // Android vuelve a llamar `surfaceCreated` al reanudar, con una
    // `ANativeWindow` nueva. Arrancar el motor otra vez machacaba el primero:
    // al escribir el segundo sobre el primero se destruía su `DesktopWrapper`,
    // con el `Editor` y el `Device` de wgpu dentro, en el hilo principal y en
    // mitad de un frame. De ahí el `abort`.
    //
    // El motor se arranca UNA vez. Después solo se rehace la superficie, que es
    // lo único que Android destruye: `wgpu::Surface` envuelve la
    // `ANativeWindow` al crearse y no hay forma de cambiársela después.
    // ------------------------------------------------------------------
    // ------------------------------------------------------------------
    // ¿SEGUNDA VEZ? ENTONCES SOLO LA SUPERFICIE.
    //
    // MEDIDO, y esto es el crash de volver del segundo plano:
    //
    //     signal 6 (SIGABRT), en el hilo principal
    //     #13 Java_dev_graphite_mobile_MainActivityKt_nativeBoot+584
    //     #19 MainActivity$callback$1.surfaceCreated+0
    //     #24 android.view.SurfaceView.surfaceCreated
    //     #23 android.view.SurfaceView.setWindowStopped
    //
    // Android vuelve a llamar `surfaceCreated` al reanudar, con una
    // `ANativeWindow` nueva. Arrancar el motor otra vez machacaba el primero:
    // al escribir el segundo sobre el primero se destruía su `DesktopWrapper`,
    // con el `Editor` y el `Device` de wgpu dentro, en el hilo principal y en
    // mitad de un frame. De ahí el `abort`.
    //
    // El motor se arranca UNA vez. Después solo se rehace la superficie, que es
    // lo único que Android destruye: `wgpu::Surface` envuelve la
    // `ANativeWindow` al crearse y no hay forma de cambiársela después.
    //
    // Se devuelve antes de nada, y con informe propio: el motor NO se vuelve a
    // sacar del `Mutex` ni a meter. Un `take()` aquí lo dejaría en `None` y el
    // siguiente mensaje del frontend se encontraría sin motor.
    // ------------------------------------------------------------------
    if ENGINE.lock().unwrap().is_some() {
        log::info!("[{}] el motor ya estaba arrancado: solo se reengancha la superficie", TAG);
        let mut guard = ENGINE.lock().unwrap();
        let engine = guard.as_mut().expect("se acaba de comprobar que hay motor");
        if let Err(msg) = engine.reenganchar_superficie(env.get_raw(), global.as_raw()) {
            log::error!("[{}] no se pudo reenganchar la superficie: {}", TAG, msg);
            drop(global);
            return string(&mut env, format!("FALLO\n{msg}"));
        }
        let estado = engine.status();
        drop(guard);
        drop(global);
        return string(&mut env, format!("superficie reenganchada — {estado}"));
    }

    let mut engine = match Engine::boot(env.get_raw(), global.as_raw()) {
        Ok(e) => e,
        Err(msg) => {
            log::error!("[{}] {}", TAG, msg);
            drop(global);
            return string(&mut env, format!("FALLO\n{msg}"));
        }
    };

    // Un frame antes de devolver: separa «el motor no arranca» de «el motor no
    // tiene tamaño». A esta altura todavía NO hay tamaño —la ventana no ha
    // avisado—, así que este frame solo evalúa el grafo y no toca la
    // superficie. Los frames de verdad empiezan tras el primer
    // `nativeSurfaceSize`.
    let primer = match engine.frame() {
        Ok(con_lienzo) => format!(
            "primer frame (sin superficie aun): lienzo={con_lienzo} — {}",
            engine.status()
        ),
        Err(e) => format!("primer frame FALLO: {e}"),
    };

    let report = format!("{}\n{}\n", engine.report, primer);
    log::info!("[{}] informe:\n{}", TAG, report);

    *ENGINE.lock().unwrap() = Some(engine);
    drop(global);

    string(&mut env, report)
}

/// **La ventana cambió de tamaño.** Reconfigura la superficie y el viewport.
///
/// Se llama desde `surfaceCreated` y desde `surfaceChanged`. El nombre de la
/// función no dice «crear» a propósito: conceptualmente es la misma operación
/// las dos veces, y separarlas fue lo que dejó el tamaño congelado.
#[export_name = "Java_dev_graphite_mobile_MainActivityKt_nativeSurfaceSize"]
pub extern "system" fn native_surface_size_jni<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    width: jint,
    height: jint,
) {
    let mut guard = ENGINE.lock().unwrap();
    let Some(engine) = guard.as_mut() else {
        log::warn!("[{}] nativeSurfaceSize({}, {}) sin motor arrancado", TAG, width, height);
        return;
    };
    log::info!("[{}] nativeSurfaceSize({}, {})", TAG, width, height);
    engine.on_surface_size(width as u32, height as u32);
}

/// Un frame.
#[export_name = "Java_dev_graphite_mobile_MainActivityKt_nativeFrame"]
pub extern "system" fn native_frame_jni<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let mut guard = ENGINE.lock().unwrap();
    let Some(engine) = guard.as_mut() else {
        return string(&mut env, "no arrancado".to_string());
    };
    // Devuelve el estado en texto, no un «ok»: hay que poder distinguir en el log
    // si el motor está renderizando o si solo está presenting negro.
    match engine.frame() {
        Ok(con_lienzo) => string(
            &mut env,
            format!("{} render_ahora={con_lienzo}", engine.status()),
        ),
        Err(e) => string(&mut env, format!("error: {e}")),
    }
}

/// Cola de mensajes del motor hacia el frontend.
///
/// MEDIDO, y es el bug que hacia que la interfaz saliera vacia:
///
///     `frontend/wrapper/src/native_communication.rs`
///
/// ```ignore
/// pub fn send_message_to_cef(message: String) {
///     let array = Uint8Array::from(message.as_bytes());
///     func.call1(&JsValue::NULL, &JsValue::from(buffer)).expect("Function call failed");
/// }
/// ```
///
/// **Ignora el valor de retorno.** El wasm no lee lo que devuelva
/// `sendNativeMessage`: manda el `ArrayBuffer` y se olvida. La respuesta vuelve
/// por OTRO camino, que es `window.receiveNativeMessage(<ArrayBuffer>)`, y en el
/// escritorio lo llama CEF con `execute_java_script`.
///
/// Este puente devolvia la respuesta como valor de retorno de JNI, asi que
/// `js -> nativo: 28 b64 entrada, 328072 b64 salida` era un dato que se
/// descartaba en la linea siguiente. MEDIDO en el movil con el contador de
/// `receiveNativeMessage`: **cero** llamadas. El frontend montaba el esqueleto de
/// la interfaz (`.main-window`, barra de titulo, barra de estado) y se quedaba
/// con todos los paneles vacios, sin un solo error en el log.
///
/// Por eso la respuesta se ENCOLA aqui y Kotlin la drena con `nativeDrain`,
/// en vez de volver por el retorno.
///
/// Cola y no un buffer compartido porque cada mensaje se entrega entero y en
/// orden: si dos hilos (el del JavaScript y el de render) drained a la vez, un
/// payload de 328 KB partido en dos haria que el JSON no se pudiera parsear.
static SALIENTE: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());

/// Encola un mensaje del motor hacia el frontend. `None` = nada que enviar.
fn encolar(respuesta: Option<Vec<u8>>) {
    let Some(respuesta) = respuesta else { return };
    match SALIENTE.lock() {
        Ok(mut cola) => cola.push_back(respuesta),
        Err(_) => log::error!("[{}] la cola de salida está envenenada", TAG),
    }
}

/// **Un mensaje del frontend**, en base64.
///
/// El shim JS recibe un `ArrayBuffer` del wasm (vía `window.sendNativeMessage`),
/// lo pasa a Kotlin como base64 —porque pasar un buffer por la interfaz de
/// JavaScript es lento y frágil— y de aquí al puente.
///
/// Base64 y no los bytes crudos porque por `JNIEnv` los `jbyteArray` hay que
/// construirlos a mano y liberarlos; con un `String` es una llamada y el
/// recolector de Java hace el resto.
///
/// **Devuelve siempre `null`.** La respuesta no vuelve por aquí sino por
/// `nativeDrain`, porque el wasm descarta el valor de retorno de
/// `sendNativeMessage`. Ver [SALIENTE].
#[export_name = "Java_dev_graphite_mobile_MainActivityKt_nativeMessage"]
pub extern "system" fn native_message_jni<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    base64: jstring,
) -> jstring {
    // `JString::from_raw` toma la propiedad del puntero. Aquí es lo correcto:
    // el `jstring` es un parámetro de la llamada, no una referencia local que
    // haya que devolver. No hay `delete_local_ref` que hacer.
    let jstr: JString<'local> = unsafe { JString::from_raw(base64) };

    let texto: String = match env.get_string(&jstr) {
        Ok(s) => s.into(),
        Err(e) => {
            log::error!("[{}] no se pudo leer el string JNI: {:?}", TAG, e);
            return std::ptr::null_mut();
        }
    };

    let datos = match base64_decode(&texto) {
        Ok(b) => b,
        Err(e) => {
            log::error!("[{}] base64 inválido: {}", TAG, e);
            return std::ptr::null_mut();
        }
    };

    let respuesta = {
        let mut guard = ENGINE.lock().unwrap();
        let Some(engine) = guard.as_mut() else {
            return std::ptr::null_mut();
        };
        engine.on_native_message(&datos)
    };

    // A la cola, no de vuelta. Ver SALIENTE: el wasm tira el valor de retorno.
    encolar(respuesta);
    std::ptr::null_mut()
}

/// Saca UN mensaje de la cola de salida, en base64, o `null` si está vacía.
///
/// Lo llama Kotlin (a) justo después de `nativeMessage`, para la respuesta a ese
/// mensaje, y (b) en el bucle de render, para lo que el motor produzca por su
/// cuenta (layouts, resultados del grafo de nodos, escenas de overlays).
///
/// UNO por llamada y no todos, para que Kotlin pueda ir entregando en orden y
/// el trabajo quede repartido entre frames en vez de bloquear un hilo con un
/// payload de 328 KB.
#[export_name = "Java_dev_graphite_mobile_MainActivityKt_nativeDrain"]
pub extern "system" fn native_drain_jni<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
) -> jstring {
    let siguiente = {
        match SALIENTE.lock() {
            Ok(mut cola) => cola.pop_front(),
            Err(_) => None,
        }
    };
    let Some(bytes) = siguiente else {
        return std::ptr::null_mut();
    };
    match base64_encode(&bytes) {
        Ok(b64) => match env.new_string(b64) {
            Ok(s) => s.into_raw(),
            Err(e) => {
                log::error!("[{}] no se pudo crear el string JNI: {:?}", TAG, e);
                std::ptr::null_mut()
            }
        },
        Err(e) => {
            log::error!("[{}] fallo al codificar la respuesta: {}", TAG, e);
            std::ptr::null_mut()
        }
    }
}

/// El frontend ha terminado de arrancar (`window.initializeNativeCommunication`).
///
/// A la cola, como `nativeMessage`, y por la misma razón: el wasm descarta el
/// valor de retorno de `sendNativeMessage`. Ver [SALIENTE].
#[export_name = "Java_dev_graphite_mobile_MainActivityKt_nativeInitialized"]
pub extern "system" fn native_initialized_jni<'local>(
    _env: JNIEnv<'local>,
    _class: JClass<'local>,
    width: jint,
    height: jint,
) -> jstring {
    let respuesta = {
        let mut guard = ENGINE.lock().unwrap();
        let Some(engine) = guard.as_mut() else {
            log::warn!("[{}] nativeInitialized sin motor arrancado", TAG);
            return std::ptr::null_mut();
        };
        engine.on_initialized(width as u32, height as u32)
    };

    encolar(respuesta);
    std::ptr::null_mut()
}

/// Base64 estándar, sin dependencias.
///
/// Se hace a mano en vez de tirar de un crate porque son quince líneas y
/// `base64` ya está en el árbol de dependencias del escritorio pero no en el del
/// puente. Una dependencia por quince líneas de decodificación no sale.
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut rev = [255u8; 256];
    for (i, c) in T.iter().enumerate() {
        rev[*c as usize] = i as u8;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for (i, c) in bytes.iter().enumerate() {
        if *c == b'=' {
            break;
        }
        // Los saltos de línea no son un error: Kotlin y JS pueden partir la
        // cadena. Descartarlos en vez de fallar es lo que evita un fallo
        // esporádico que solo aparece con mensajes grandes.
        if *c == b'\n' || *c == b'\r' {
            continue;
        }
        let v = rev[*c as usize];
        if v == 255 {
            return Err(format!(
                "carácter inválido {:?} en la posición {}",
                *c as char, i
            ));
        }
        acc = (acc << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push(((acc >> nbits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

fn base64_encode(datos: &[u8]) -> Result<String, String> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(datos.len().div_ceil(3) * 4);
    for chunk in datos.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(T[((n >> 18) & 63) as usize] as char);
        s.push(T[((n >> 12) & 63) as usize] as char);
        s.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        s.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    Ok(s)
}

fn init_logger() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag(TAG),
    );

    // ------------------------------------------------------------------
    // `tracing` -> `log` -> logcat. ESTO NO ESTABA, Y ES GRANDE.
    //
    // MEDIDO: `grep -rn 'tracing_subscriber' android/bridge/src/` no devuelve
    // NADA. Upstream usa `tracing::` por todas partes —`tracing::error!`,
    // `tracing::warn!`— y sin un subscriber registrado cada uno de esos eventos
    // se descarta en silencio. Entre otros:
    //
    //   desktop/src/render/state.rs:238  tracing::error!("Error rendering overlays: {:?}")
    //
    // O sea: los diagnosticos del motor no existen para nosotros. Si el motor
    // esta fallando al rasterizar y avisando, no hay forma de verlo, y el unico
    // sintoma es que en la pantalla no aparece nada.
    //
    // Que es exactamente lo que pasa. MEDIDO en el movil, y con una prueba que
    // no se me habia ocurrido: al tocar con la herramienta de seleccion donde NO
    // hay nada, el color de relleno se pone blanco, y al tocar donde se supone
    // que hay el rectangulo se pone ROJO. O sea que el hit-test encuentra el
    // rectangulo exactamente donde se dibujo, y el hit-test usa la misma
    // transformada que el render. La geometria esta bien y el fallo es de
    // render, "sin ningun error en el log que lo diga" — porque los errores del
    // motor se estaban tirando a la basura antes de existir.
    //
    // `LogTracer` convierte los eventos de `tracing` en registros de `log`, que
    // es lo que `android_logger` ya sabe escribir. Asi todo sale con NUESTRO tag
    // y con el filtro de nivel de siempre, en vez de tener dos sistemas de log
    // paralelos.
    //
    // `try_init` y no `init`: `native_boot` se llama en cada `surfaceCreated`, y
    // un segundo `init` reventado seria un panic en el hilo principal. Con
    // `surfaceCreated`又名 siendo llamado en cada reanudar, eso no es teorico.
    //
    let _ = tracing_log::LogTracer::init(log::LevelFilter::Info);
    // `warn!` del motor con prefijo, y `info!` de Graphite tambien salen. Sin
    // esto, un `tracing::info!` de la biblioteca se pierde igual que un error.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,graphite=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    // En Android los panics de Rust **NO** llegan a logcat: van a stderr, y stderr
    // está cerrado. Sin este hook, un panic aborta sin dejar ni una línea. Pasó
    // dos veces en el spike anterior y costó dos builds enteros.
    std::panic::set_hook(Box::new(|info| {
        let msg = match info.payload().downcast_ref::<&str>() {
            Some(s) => (*s).to_string(),
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => s.clone(),
                None => "pánico sin mensaje".into(),
            },
        };
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "sin ubicación".into());
        log::error!("[{}] PÁNICO en {}: {}", TAG, loc, msg);
    }));
}