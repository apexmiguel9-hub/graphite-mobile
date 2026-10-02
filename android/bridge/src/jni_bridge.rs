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

use jni::objects::{JClass, JObject};
use jni::sys::{jint, jstring};
use jni::JNIEnv;

use crate::{Engine, TAG};

/// Una sola instancia del motor para toda la app.
static ENGINE: std::sync::Mutex<Option<Engine>> = std::sync::Mutex::new(None);

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

fn init_logger() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag(TAG),
    );
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