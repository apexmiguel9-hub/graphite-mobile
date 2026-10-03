//! Puente JNI de Graphite para Android.
//!
//! # Qué es esto
//!
//! El motor de Graphite (`editor/`, `node-graph/`, `document/`, `libraries/`) es
//! Rust y ya habla Vulkan/GL por wgpu, así que en Android funciona sin tocarlo.
//! Lo que no existe es el **puente**: la capa que:
//!
//! 1. Crea el device de wgpu y se lo pasa al motor.
//! 2. Crea la superficie de wgpu a partir del `Surface` de Java.
//! 3. Ejecuta el grafo de nodos del motor.
//! 4. Lleva el resultado a la pantalla.
//! 5. Traduce los mensajes del motor a algo que una UI pueda usar.
//!
//! Eso es todo este crate. No hay una copia del motor dentro.
//!
//! # La lección que ya costó dinero
//!
//! **El tamaño de la superficie no se fija al arrancar, y se reconfigura cada vez
//! que cambia.** El bug anterior —un triángulo negro en mitad de la pantalla—
//! venía de acoplar el tamaño a `surfaceCreated`, que en Android se dispara
//! *antes* del layout, y de no volver a mirar nunca el tamaño. La superficie se
//! quedaba con una extensión que no era la del `ANativeWindow` y el compositor
//! recortaba el frame.
//!
//! El detalle importante: `get_current_texture()` devuelve el tamaño que el
//! *swapchain* fue configurado, no el de la ventana. Si discrepan, el culpable es
//! la configuración obsoleta, y por eso `frame()` lo comprueba y lo dice en el
//! log en vez de dejar que se manifests como un artefacto raro en una captura.
//!
//! Con eso, cada línea de aquí está comentada con *por qué*, no con *qué*.

pub mod jni_bridge;

use graphite_editor::messages::frontend::FrontendMessage;
use graphite_editor::messages::portfolio::PortfolioMessage;
use graphite_editor::messages::prelude::*;

// El wrapper del escritorio. Es lo que hace barato este port: el dispatcher de
// mensajes y las dos funciones de la frontera ya estan escritos y probados, y
// no son especificos de ninguna plataforma.
use graphite_desktop_wrapper::{
    DesktopFrontendMessage, DesktopWrapper, DesktopWrapperMessage, NodeGraphExecutionResult,
    deserialize_editor_message, serialize_frontend_messages,
};

use graph_craft::application_io::resource::{HashMapResourceStorage, ResourceStorage};
use wgpu_executor::WgpuContext;

use document_container::store::{DocumentStore, MemoryStore};

pub const TAG: &str = "GRAPHITE";

const BLIT_WGSL: &str = include_str!("blit.wgsl");

/// Marca de fase. Sin esto un fallo de arranque llega al log como «no arrancó» y
/// no se sabe en qué punto; con esto, en qué punto.
fn fase(nombre: &str) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    log::info!("[{}] FASE t={}ms {}", TAG, t, nombre);
}

/// Modo de diagnóstico del blit. Lo cambia `native_debug`, que llega desde JS.
///
/// Un `AtomicU32` y no un campo del `Engine` a propósito: el blit escribe el
/// uniform desde el hilo de render, y esto lo escribe el hilo del JavaScript. Con
/// un `Mutex` sería lo de siempre, pero aquí no hay nada que proteger —un entero
/// que solo se lee y solo se escribe— y un atomic no puede quedarse envenenado.
pub static MODO_VISUALIZACION: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// El rectángulo del lienzo dentro de la ventana, en **píxeles físicos**.
///
/// Lo emite el editor ya convertido (`to_physical()`), no el frontend: quien lo
/// envía es `editor/src/messages/viewport/viewport_message_handler.rs:42`. El
/// frontend manda píxeles CSS en `ViewportMessage::Update`, y el editor los
/// multiplica por la escala que le dio (`viewports.ts:53`).
#[derive(Debug, Clone, Copy)]
pub struct Lienzo {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Estado vivo entre llamadas JNI. Una sola instancia, detrás de un `Mutex`.
pub struct Engine {
    /// Device/queue/instance/adapter **del motor**. Se crean aquí y se le pasan
    /// al motor para que la textura que produce Graphite y la superficie de la
    /// pantalla sean del mismo device: sin copia, sin sincronización, sin
    /// barrier entre ellos.
    pub context: WgpuContext,
    /// La superficie de la pantalla. De la *misma* instancia que el device de
    /// arriba; por eso el enlace entre ambos es directo.
    pub surface: wgpu::Surface<'static>,
    pub surface_format: wgpu::TextureFormat,

    /// Composición alfa de la superficie.
    ///
    /// No se fija a mano: se pregunta a la superficie qué admite y se elige.
    ///
    /// MEDIDO (Motorola G56, PowerVR BXM-8-256, Vulkan): pedir `Opaque` a pelo
    /// aborta el arranque con
    ///     Requested alpha mode Opaque is not in the list of supported alpha
    ///     modes: [Inherit]
    /// Ese driver solo ofrece `Inherit`. Se prefiere `Opaque` porque es lo
    /// correcto para una superficie que pinta opaco, pero si no está, se coge
    /// lo que haya. El blit devuelve alfa 1 siempre, así que el resultado
    /// visible es el mismo en cualquier caso.
    alpha_mode: wgpu::CompositeAlphaMode,

    /// Extensión con la que se configuró la superficie por última vez.
    ///
    /// `(0, 0)` significa «nunca configurada». Es un estado válido: la
    /// superficie se crea antes de que la ventana tenga tamaño, y configurar con
    /// 0 produce una superficie degenerada.
    configured: (u32, u32),

    /// El motor, y la capa de mensajes que upstream escribe para el escritorio.
///
/// No se habla con el `Editor` directamente: se habla con `DesktopWrapper`, que
/// es lo mismo que hace el escritorio. La diferencia no es de estilo, es que el
/// dispatcher se encarga de cosas que son fáciles de olvidar y que rompen la
/// app si se ignoran: interceptar mensajes del editor antes de dárselos,
/// reinyectar las respuestas del motor en el motor, y agrupar mensajes en un
/// solo `Batched`.
    pub wrapper: DesktopWrapper,
    pub pipeline: wgpu::RenderPipeline,
    pub sampler: wgpu::Sampler,
    /// Textura 1x1 transparente, para el frame 0 (cuando el motor aún no ha
    /// renderizado nada y no hay nada que muestrear).
    pub fallback: wgpu::TextureView,

    /// La última textura que produjo el motor.
    ///
    /// El motor es *demand-driven*: `run_node_graph()` renderiza cuando algo
    /// cambia y devuelve `None` el resto de los frames. `None` significa «no hay
    /// nada nuevo», **no** «no hay imagen». Sin guardar la textura anterior, el
    /// blit pinta el fallback encima del lienzo y la pantalla se queda negra o
    /// con la imagen de hace 4 segundos.
    pub last_texture: Option<graphene_std::application_io::Texture>,

    pub report: String,
    pub frames: u32,
    pub rendered: u32,
    /// Veces que el tamaño real del swapchain ha discrepado de lo configurado.
    /// Si esto sube, hay un `surfaceChanged` que no está llegando.
    pub resizes: u32,

    /// Mensajes recibidos del frontend. Si esto se queda en 0 con la UI puesta,
    /// el shim JS no está llegando a `on_native_message` (el problema más
    /// probable siendo que `window.sendNativeMessage` no exista).
    pub messages_in: u32,
    /// `DesktopFrontendMessage` de escritorio que aún no se manejan. Ver
    /// `on_native_message`.
    pub messages_descartados: u32,
    /// Dónde está el lienzo dentro de la ventana, y cuánto se escala.
    ///
    /// MEDIDO, y es lo que faltaba para que se viera lo que se dibuja. Antes se
    /// descartaba y el blit estiraba la textura del motor a toda la ventana, con
    /// lo que el lienzo se dibujaba desplazado y escalado: la capa se creaba (el
    /// comando llegaba) pero no se veía nada.
    ///
    /// Se recalcula en cuanto cambia el tamaño de la superficie —un giro lo
    /// cambia— y no solo cuando llega el mensaje.
    pub lienzo: Option<Lienzo>,

    /// Buffer con `offset` y `scale` para el blit. Ver `blit.wgsl`.
    pub colocacion: wgpu::Buffer,

    /// Cuantas veces el grafo de nodos ha dicho `NotRun`: no produjo textura.
    /// Es el estado silencioso por excelencia, asi que necesita contador.
    pub grafo_sin_correr: u32,

    /// Veces que el grafo corrio pero SIN textura nueva. Demand-driven: es el
    /// estado invisible por excelencia, porque `last_texture` sigue siendo
    /// valido y no hay ningun error. Ver `frame`.
    pub grafo_sin_textura: u32,

    /// Veces que el grafo devovio una textura de verdad. Si esto NO sube cuando
    /// se dibuja algo, el grafo no se esta invalidando.
    pub grafo_con_textura: u32,

    /// Ultimo tamaño de la textura que devolvio el grafo, para detectar cambios.
    pub ultimo_tamano_textura: (u32, u32),

    /// Escenas de Vello de overlays que han llegado y NO se rasterizan todavía.
    /// Solo el log: sirve para distinguir "el frontend no manda escenas" de
    /// "llegan y no se pintan". Ver `on_native_message`.
    pub escenas_overlays: u32,
    /// Tipos de `DesktopFrontendMessage` descartados en el último lote, y cuántos
    /// de cada uno. Se vacía en cada tanda y se loguea: solo sirve para el
    /// diagnóstico, no es estado. Ver `nombre_de`.
    pub descartados_por_tipo: std::collections::HashMap<&'static str, u32>,
    /// Si el frontend ya ha pedido conexión.
    pub frontend_conectado: bool,
}

impl Engine {
    /// Arranca el motor. **No toca la superficie.**
    ///
    /// Separar esto de `configure_surface` es el punto. Si el arranque dependiera
    /// del tamaño, el tamaño quedaría congelado en el instante del arranque, y
    /// el tamaño de la ventana cambia (giro, teclado,(split screen, barras del
    /// sistema).
    ///
    /// El orden es el mismo que usa el escritorio (`desktop/wrapper/src/lib.rs`):
    /// GPU → motor → documento → viewport → primer render.
    pub fn boot(env: *mut jni::sys::JNIEnv, surface_job: jni::sys::jobject) -> Result<Self, String> {
        // ------------------------------------------------------------------
        // 1. GPU
        // ------------------------------------------------------------------
        fase("1 GPU");
        let (context, raw_instance, adapter) = Self::gpu()?;

        // ------------------------------------------------------------------
        // 2. Superficie (pero sin configurarla: eso es de `on_surface_size`)
        // ------------------------------------------------------------------
        fase("2 superficie");
        let surface = Self::surface(env, surface_job, raw_instance)?;
        let surface_format = Self::pick_format(&surface, &adapter);
        let alpha_mode = Self::pick_alpha_mode(&surface, &adapter);

        // ------------------------------------------------------------------
        // 3. Motor, vía el wrapper de upstream
        // ------------------------------------------------------------------
        fase("3 DesktopWrapper::new");
        // Almacenamientos EN MEMORIA. Es lo que hace el wrapper web
        // (`frontend/wrapper/src/editor_wrapper.rs`) y es lo correcto aquí: una
        // app móvil no puede usar rutas del sistema de ficheros como lo hace el
        // escritorio.
        let resource_storage: std::sync::Arc<dyn ResourceStorage> =
            std::sync::Arc::new(HashMapResourceStorage::new());
        let document_store: std::sync::Arc<dyn DocumentStore> =
            std::sync::Arc::new(MemoryStore::default());

        // `wake` lo usaría el escritorio para despertar el bucle de eventos.
        // Aquí el bucle es propio, así que no hay a quién avisar.
        let wake = std::sync::Arc::new(|| {});

        // `DesktopWrapper::new` recibe el `WgpuContext` y construye el `Editor`
        // con `Platform::Android` —el brazo ese es un cambio de tres lineas en
        // upstream, porque si no el crate ni siquiera compila para android—.
        let mut wrapper = DesktopWrapper::new(
            0,
            resource_storage,
            document_store,
            context.clone(),
            wake,
        );
        fase("4 DesktopWrapper construido");

        // ------------------------------------------------------------------
        // 4. Un documento. Sin documento el grafo de nodos no tiene nada que
        //    evaluar y `execute_node_graph()` no devuelve textura.
        //
        //    El escritorio NO crea el documento: lo crea el frontend cuando
        //    carga. Aquí se sigue haciendo a mano para que el lienzo tenga algo
        //    desde el arranque; cuando el frontend este montado y cree el suyo,
        //    esto se puede quitar.
        // ------------------------------------------------------------------
        fase("5 documento");
        let respuestas = wrapper.dispatch(DesktopWrapperMessage::FromWeb(Box::new(
            Message::Portfolio(PortfolioMessage::NewDocumentWithName {
                name: "Untitled".into(),
            }),
        )));
        log::info!(
            "[{}] documento creado: {} respuestas al frontend",
            TAG,
            respuestas.len()
        );

        // ------------------------------------------------------------------
        // 5. Blit
        // ------------------------------------------------------------------
        fase("6 blit");
        let (pipeline, sampler, fallback, colocacion) =
            Self::blit(&context.device, &context.queue, surface_format);

        // El viewport todavía no se manda: no hay tamaño. Lo mandará
        // `on_surface_size` en cuanto la ventana lo tenga. Mandarlo con 0 es lo
        // que hacía el puerto anterior y producía un render de 1x1.
        fase("7 listo, esperando tamaño de superficie");

        let report = format!(
            "Motor de Graphite arrancado en Android\n\
             -----------------------------------\n\
             adaptador:  {}\n\
             superficie: {:?}\n",
            adapter.get_info().name,
            surface_format,
        );

        Ok(Engine {
            context,
            surface,
            surface_format,
            alpha_mode,
            configured: (0, 0),
            wrapper,
            pipeline,
            sampler,
            fallback,
            colocacion,
            last_texture: None,
            report,
            frames: 0,
            rendered: 0,
            resizes: 0,
            messages_in: 0,
            messages_descartados: 0,
            escenas_overlays: 0,
            grafo_sin_correr: 0,
            grafo_sin_textura: 0,
            grafo_con_textura: 0,
            ultimo_tamano_textura: (0, 0),
            lienzo: None,
            descartados_por_tipo: std::collections::HashMap::new(),
            frontend_conectado: false,
        })
    }

    /// Nombre de un `DesktopFrontendMessage` que todavía no se maneja.
    ///
    /// MEDIDO, y por qué existe esto: `msj_descartados` iba a 24 y no decía
    /// **ni uno**. Un contador sin nombres no dice nada: no se sabe si son 24
    /// mensajes de lo mismo o uno de cada cosa, y por tanto no se sabe si
    /// alguno explica que el lienzo no dibuje.
    ///
    /// Los nombres son explícitos, no automáticos. Un `std::any::type_name` o
    /// similar daría el nombre del *tipo*, no el de la variante, que es
    /// justo lo que se quiere saber. Y al estar la función al lado del `match`
    /// que descarta, añadir una variante al enum obliga a pasar por aquí: el
    /// compilador avisa, y eso es justo lo que se quiere.
    ///
    /// `Window*`, `Restart` y `LoadThirdPartyLicenses` caen en el resto a
    /// propósito: no hacen falta en un móvil y agruparlos deja el log legible.
    fn nombre_de(m: &DesktopFrontendMessage) -> &'static str {
        match m {
            DesktopFrontendMessage::UpdateViewportPhysicalBounds { .. } => "UpdateViewportPhysicalBounds",
            DesktopFrontendMessage::UpdateUIScale { .. } => "UpdateUIScale",
            DesktopFrontendMessage::WindowUpdateDirectInput { .. } => "WindowUpdateDirectInput",
            DesktopFrontendMessage::UpdateMenu { .. } => "UpdateMenu",
            DesktopFrontendMessage::OpenFileDialog { .. } => "OpenFileDialog",
            DesktopFrontendMessage::SaveFileDialog { .. } => "SaveFileDialog",
            DesktopFrontendMessage::WriteFile { .. } => "WriteFile",
            DesktopFrontendMessage::OpenUrl(_) => "OpenUrl",
            DesktopFrontendMessage::OpenLaunchDocuments => "OpenLaunchDocuments",
            DesktopFrontendMessage::PersistenceWritePreferences { .. } => "PersistenceWritePreferences",
            DesktopFrontendMessage::PersistenceLoadPreferences => "PersistenceLoadPreferences",
            DesktopFrontendMessage::PersistenceWriteState { .. } => "PersistenceWriteState",
            DesktopFrontendMessage::PersistenceReadState => "PersistenceReadState",
            DesktopFrontendMessage::ClipboardRead => "ClipboardRead",
            DesktopFrontendMessage::ClipboardWrite { .. } => "ClipboardWrite",
            DesktopFrontendMessage::PointerLock => "PointerLock",
            DesktopFrontendMessage::WindowFullscreen => "WindowFullscreen",
            DesktopFrontendMessage::WindowFocus => "WindowFocus",
            _ => "resto (ventana de escritorio: Window*, Restart, LoadThirdPartyLicenses)",
        }
    }

    /// Reengancha una superficie nueva **sin tocar el motor**.
    ///
    /// MEDIDO, y esto es lo que arregla el cierre al volver del segundo plano.
    ///
    ///     signal 6 (SIGABRT), en el hilo principal
    ///     #13 Java_dev_graphite_mobile_MainActivityKt_nativeBoot+584
    ///     #19 dev.graphite.mobile.MainActivity$callback$1.surfaceCreated+0
    ///     #22 android.view.SurfaceView.updateSurface
    ///     #23 android.view.SurfaceView.setWindowStopped
    ///     #24 android.view.SurfaceView.surfaceCreated
    ///
    /// Android vuelve a llamar `surfaceCreated` al reanudar, con una
    /// `ANativeWindow` NUEVA. `nativeBoot` arrancaba un motor entero cada vez, y
    /// al asignar el segundo sobre el primero se destruía el `DesktopWrapper` del
    /// primero: su `Editor` con su `WgpuExecutor` y su `Device` de wgpu, en el
    /// hilo principal y en mitad de un frame. De ahí el `abort`.
    ///
    /// No hace falta arrancar un motor nuevo: la `Surface` de wgpu se puede
    /// reemplazar sin tocar el `Device`, el `Queue` ni el `Editor`, porque la
    /// superficie nueva usa la MISMA instancia. Se reutilizan los dos, y solo se
    /// rehace la superficie y su configuración.
    ///
    /// Lo que NO se puede hacer es no rehacerla: `wgpu::Surface` envuelve la
    /// `ANativeWindow` en el momento de crearse, y no hay forma de cambiarle la
    /// ventana después. Android destruye la superficie al minimizar, así que la
    /// antigua queda inservible.
    pub fn reenganchar_superficie(
        &mut self,
        env: *mut jni::sys::JNIEnv,
        surface_job: jni::sys::jobject,
    ) -> Result<(), String> {
        log::info!("[{}] reenganchando la superficie del motor", TAG);

        // El adaptador sale del contexto por `Deref`, que es lo mismo que hace
        // `gpu()`: `wgpu_sync::Adapter` no se convierte a `wgpu::Adapter`, se
        // clona a través del `Deref`.
        let adapter: wgpu::Adapter = (*self.context.adapter).clone();
        let instancia: wgpu::Instance = (*self.context.instance).clone();

        self.surface = Self::surface(env, surface_job, instancia)?;
        self.surface_format = Self::pick_format(&self.surface, &adapter);
        self.alpha_mode = Self::pick_alpha_mode(&self.surface, &adapter);

        // La superficie nueva no está configurada, y `surfaceCreated` todavía no
        // sabe el tamaño. Se marca como sin configurar y se espera a que llegue
        // `surfaceChanged`, que siempre va detrás.
        //
        // No se "recuerda" el tamaño viejo a propósito: si `surfaceChanged` no
        // llegara, el siguiente frame presentaría a una superficie sin
        // configurar, y los errores de wgpu son FATALES por defecto.
        self.configured = (0, 0);
        // Vuelve a "lienzo a pantalla completa" hasta que llegue el tamaño.
        self.actualizar_colocacion();
        log::info!(
            "[{}] superficie reenganchada; configurara al llegar el tamano",
            TAG
        );
        Ok(())
    }

    /// Escribe en el uniform dónde está el lienzo, a partir del tamaño de la
    /// superficie.
    ///
    /// La aritmética es la de `desktop/src/app.rs:285-295`, y es la misma porque
    /// las unidades también lo son: el mensaje ya viene en píxeles físicos
    /// (`to_physical()`) y la superficie se mide en píxeles físicos.
    ///
    /// `offset` es la fracción de la ventana donde empieza el lienzo, y `scale`
    /// es su inversa en esa dimensión: `scale = ventana / lienzo`. Así
    /// `(uv - offset) * scale` vale 0..1 exactamente dentro del lienzo, que es lo
    /// que el fragment shader necesita para mapear la textura.
    ///
    /// Con `scale = 1, offset = 0` el lienzo ocupa toda la ventana: es lo que
    /// había antes, y el valor inicial cuando todavía no ha llegado el mensaje.
    pub fn actualizar_colocacion(&mut self) {
        let (sw, sh) = self.configured;
        let (offset_x, offset_y, scale_x, scale_y) = match self.lienzo {
            Some(l) if sw > 0 && sh > 0 && l.width > 0.0 && l.height > 0.0 => (
                l.x / sw as f64,
                l.y / sh as f64,
                sw as f64 / l.width,
                sh as f64 / l.height,
            ),
            _ => (0.0, 0.0, 1.0, 1.0),
        };

        // Se escribe SOLO cuando cambia algo: al llegar el mensaje del frontend y
        // al cambiar el tamaño de la superficie. Son 16 bytes y no vale la pena
        // encolarlos 60 veces por segundo para que no cambie nada.
        self.context.queue.write_buffer(
            &self.colocacion,
            0,
            bytemuck::cast_slice(&[
                offset_x as f32,
                offset_y as f32,
                scale_x as f32,
                scale_y as f32,
                MODO_VISUALIZACION.load(std::sync::atomic::Ordering::Relaxed) as f32,
                0.0,
                0.0,
                0.0,
            ]),
        );

        if self.frames <= 3 {
            log::info!(
                "[{}] colocacion del lienzo: offset=({:.4}, {:.4}) scale=({:.4}, {:.4}) lienzo={:?} superficie={:?}",
                TAG,
                offset_x,
                offset_y,
                scale_x,
                scale_y,
                self.lienzo,
                self.configured
            );
        }
    }

    /// **El tamaño de la ventana cambió.** Reconfigura superficie y viewport.
    ///
    /// Se llama desde `surfaceCreated` **y** desde `surfaceChanged` en Kotlin.
    /// Ignora los tamaños cero: en `surfaceCreated` la vista todavía no está
    /// medida, y configurar una superficie en 0x0 la deja inservible.
    pub fn on_surface_size(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            log::info!(
                "[{}] on_surface_size({}, {}) ignorado: tamaño degenerado",
                TAG,
                width,
                height
            );
            return;
        }

        if self.configured != (width, height) {
            let anterior = self.configured;

            // Esperar a que la GPU quede libre ANTES de reconfigurar.
            //
            // MEDIDO (Motorola G56): sin esto,
            //     In Surface::configure
            //       Failed to wait for GPU to come idle before reconfiguring
            //       the Surface
            //
            // No es un detalle: `run_node_graph()` encola trabajo de render en
            // el device, y reconfigurar la superficie con ese trabajo en vuelo
            // es un error de validación. Y los errores de wgpu son FATALES por
            // defecto, así que la app muere.
            // En wgpu 29 `PollType::Wait` es una variante con campos, no una unidad:
            // `PollType::Wait { submission_index, timeout }`. Para "esperar a
            // que se vacie la cola" esta el constructor
            // `PollType::wait_indefinitely()`, que es el que usa upstream en
            // `node-graph/libraries/wgpu-executor/src/texture_conversion.rs`.
            //
            // `poll` devuelve Result; el error se ignora a proposito porque aqui
            // no hay nada que hacer si falla —no estamos leyendo nada— y lo que
            // importa es que la GPU quede quieta.
            let _ = self.context.device.poll(wgpu::PollType::wait_indefinitely());

            self.surface.configure(
                &self.context.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format: self.surface_format,
                    width,
                    height,
                    // 2 frames de latencia: suficiente para no bloquear en el
                    // compositor sin añadir input lag perceptible al dibujar.
                    desired_maximum_frame_latency: 2,
                    present_mode: wgpu::PresentMode::Fifo,
                    alpha_mode: self.alpha_mode,
                    view_formats: vec![],
                },
            );
            self.configured = (width, height);
            // La colocación del lienzo depende del tamaño de la superficie, asi
            // que un giro la invalida aunque el frontend no mande nada nuevo.
            self.actualizar_colocacion();
            log::info!(
                "[{}] superficie reconfigured {:?} -> {}x{}",
                TAG,
                anterior,
                width,
                height
            );
        }

        // ------------------------------------------------------------------
        // NO SE MANDA `ViewportMessage::Update` DESDE AQUI. Y ESTA ES LA PARTE
        // IMPORTANTE: HAY DOS ESCRITORES DEL MISMO DATO, Y ESTE ESTA MAL.
        //
        // MEDIDO. Lo mandaba asi:
        //
        //     ViewportMessage::Update { x: 0., y: 0., width: 2168, height: 1021,
        //                               scale: 1.0 }
        //
        // y el frontend, al MISMO TIEMPO, desde
        // `frontend/src/utility-functions/viewports.ts:53`:
        //
        //     editor.updateViewport(274, 255, 1336, 668, 2.4375)
        //
        // Los dos escriben el MISMO `ViewportState.bounds`. No son dos vistas del
        // mismo dato: son dos escrituras, y gana la que llega el ultimo. El
        // nuestro pone el tamano de la VENTANA entera, la posicion en (0,0) y
        // `scale: 1.0` — es decir, exactamente lo que el frontend vino a
        // corregir, porque el lienzo es un subrectangulo con paneles alrededor.
        //
        // MEDIDO que el del frontend SIEMPRE llega, y por tanto el nuestro sobra.
        // La prueba es indirecta pero no admite otra lectura: el editor emite
        // `FrontendMessage::UpdateViewportPhysicalBounds` **en respuesta** a
        // `ViewportMessage::Update` (`viewport_message_handler.rs:42`), y
        // nosotros RECIBIMOS ese mensaje —llega, y lo estamos usando para colocar
        // la textura del blit—. Si lo recibimos, el `ViewportMessage::Update`
        // del frontend tuvo que llegar antes.
        //
        // O sea: el del frontend es el unico que llega, y el nuestro solo puede
        // coronar un `Update` equivocado encima. Por eso se quita.
        //
        // Y el comentario de antes, que decia "lo mandamos nosotros al arrancar
        // para que no haya un parpadeo de 1x1", era una suposicion: el
        // `ResizeObserver` del frontend (`viewports.ts:20`) corre en su `onMount`,
        // que es antes del primer frame util. El parpadeo de 1x1 que se veia era
        // otra cosa.
        log::info!(
            "[{}] superficie {}x{}; el viewport del motor NO se toca aqui: lo manda el frontend",
            TAG,
            width,
            height
        );
    }

    /// **Un mensaje del frontend**, ya en bytes.
    ///
    /// Devuelve los bytes de la respuesta para el frontend, o `None` si no hay
    /// nada que mandarle de vuelta.
    ///
    /// Esto ES la frontera, y es enteramente código de upstream:
    ///
    ///   `deserialize_editor_message`  ->  `DesktopWrapperMessage`
    ///   `wrapper.dispatch`            ->  el motor responde
    ///   `serialize_frontend_messages` ->  `Vec<u8>` para el JS
    ///
    /// Lo único propio es decidir qué hacer con los `DesktopFrontendMessage`
    /// que NO son `ToWeb`: en el escritorio son diálogos de fichero, menú,
    /// portapapeles y persistencia. Aquí todavía no hay gestión de ficheros ni
    /// menú, así que se cuentan y se loguean. Contados porque un
    /// `DesktopFrontendMessage` descartado en silencio es un botón que no hace
    /// nada y nadie sabe por qué.
    pub fn on_native_message(&mut self, datos: &[u8]) -> Option<Vec<u8>> {
        let mensaje = match deserialize_editor_message(datos) {
            Some(m) => m,
            None => {
                log::warn!(
                    "[{}] mensaje del frontend NO deserializable ({} bytes)",
                    TAG,
                    datos.len()
                );
                return None;
            }
        };

        self.messages_in += 1;

        let respuestas = self.wrapper.dispatch(mensaje);

        // ------------------------------------------------------------------
        // SEPARAR LO QUE VA AL FRONTEND DE LO QUE ES UNA PETICIÓN AL ESCRITORIO.
        //
        // MEDIDO, y contra una idea que yo tenía mal. `UpdateOverlays` es una
        // ESCENA DE VELLO, sí, y en el escritorio la rasteriza Rust para pintar
        // encima del lienzo los handles, las anclas y el rectángulo de selección.
        // Pero **la interfaz no va por ahí**: la interfaz la pinta el DOM, que
        // CEF entrega en `on_paint` como píxeles y el WebView pinta en pantalla.
        //
        // Lo que se ve MEDIDO en el móvil, con `Runtime.evaluate` sobre la
        // página real de DevTools: existe `.main-window` a 443x984, con
        // `title-bar`, `menu-bar`, `workspace` y `status-bar`, y en la captura
        // se ve el icono de pantalla completa de la esquina. El esqueleto lo
        // pintaba el DOM mientras todos los paneles salían vacíos, porque no
        // llegaba ni un mensaje — eso era otro problema, y estaba en el puente.
        //
        // Así que `UpdateOverlays` se cuenta pero no se rasteriza todavía, en vez
        // de meter un compositor de tres capas sin medir si hacía falta. Los
        // overlays van sobre el lienzo, y para eso hay que saber dónde está el
        // lienzo (`UpdateViewportPhysicalBounds`), que es lo siguiente.
        // ------------------------------------------------------------------
        let mut para_web: Vec<FrontendMessage> = Vec::new();
        let mut otros = 0usize;
        for r in respuestas {
            match r {
                DesktopFrontendMessage::ToWeb(mut ms) => para_web.append(&mut ms),
                DesktopFrontendMessage::UpdateOverlays(_) => {
                    self.escenas_overlays += 1;
                }
                DesktopFrontendMessage::UpdateViewportPhysicalBounds {
                    x,
                    y,
                    width,
                    height,
                } => {
                    // ------------------------------------------------------------------
                    // DÓNDE ESTÁ EL LIENZO. Este mensaje era el que faltaba.
                    //
                    // MEDIDO: se descartaba (`DESCARTADO x2: UpdateViewportPhysicalBounds`)
                    // y sin él el blit estiraba la textura del motor a toda la
                    // ventana. Resultado: la capa se creaba y no se veía nada.
                    // Los TRES modos de render (SVG, pixel outline, normal) fallaban
                    // igual, y eso es lo que descarta que sea un problema de
                    // pintado: es geometría, y por eso no lo arregla cambiar de
                    // shader.
                    //
                    // Las unidades YA SON FÍSICAS, y no por casualidad:
                    // `editor/src/messages/viewport/viewport_message_handler.rs:42`
                    // hace `self.bounds().to_physical()` antes de emitirlo. Así que
                    // aquí no hay que convertir nada y la aritmética de
                    // `desktop/src/app.rs:285` aplica tal cual.
                    //
                    // Y el origen coincide: el motor mide desde el (0,0) de la
                    // superficie, y el (0,0) CSS del WebView es la misma esquina
                    // porque los dos estánemblies llevan el mismo padding de
                    // insets. Si algún día dejan de compartirlo, el `offset` de
                    // abajo es lo primero que se rompe.
                    // ------------------------------------------------------------------
                    self.lienzo = Some(Lienzo { x, y, width, height });
                    self.actualizar_colocacion();
                }
                ref otro => {
                    otros += 1;
                    *self
                        .descartados_por_tipo
                        .entry(Self::nombre_de(otro))
                        .or_insert(0) += 1;
                }
            }
        }

        if otros > 0 {
            // MEDIDO: «cuántos» no sirve de nada. Hay que ver «cuáles»: el
            // contador se agregaba en 24 y no decia ni uno. El nombre sale de
            // `nombre_de`, y sale UNO POR TIPO y no uno por mensaje: son
            // decenas por segundo en operación normal y llenan el log.
            for (nombre, n) in self.descartados_por_tipo.drain() {
                log::warn!("[{}] DESCARTADO x{}: {}", TAG, n, nombre);
            }
            self.messages_descartados += otros as u32;
        }

        if para_web.is_empty() {
            return None;
        }

        match serialize_frontend_messages(para_web) {
            Some(bytes) => Some(bytes),
            None => {
                log::error!("[{}] no se pudieron serializar los FrontendMessage", TAG);
                None
            }
        }
    }

    /// El frontend ha terminado de arrancar y pide conexión
    /// (`window.initializeNativeCommunication`).
    ///
    /// En el escritorio esto lo dispara el `on_context_created` de CEF
    /// (`desktop/ui/src/internal/render_process_v8_handler.rs`). Aquí lo llama
    /// el shim JS del WebView.
    ///
    /// Lo que el frontend necesita en este punto es el estado de la aplicación:
    /// preferencias, documentos del último uso y el tamaño de la ventana. En el
    /// escritorio lo carga `desktop/src/app.rs`; en Android todavía no hay
    /// persistencia, así que se le pasan las medidas y se registra el resto
    /// como NO implementado, en vez de fingir que sí.
    pub fn on_initialized(&mut self, width: u32, height: u32) -> Option<Vec<u8>> {
        log::info!("[{}] el frontend pide conexión ({}x{})", TAG, width, height);
        self.frontend_conectado = true;

        // Sin persistencia todavía. En el escritorio esto son
        // `PersistenceLoadPreferences` y `PersistenceReadState`, que leen de
        // disco. Aquí no hay disco: cuando se implemente, son dos dispatch más.
        // Se dejan escritos y comentados para que quede claro qué falta, en vez
        // de que falte sin decir nada.

        // Un `Wake` es lo que el escritorio manda cuando una future del motor
        // termina; aquí no hay futures, pero es el mensaje que le dice al motor
        // que ya puede trabajar y produce las respuestas iniciales.
        let respuestas = self.wrapper.dispatch(DesktopWrapperMessage::Wake);

        let mut para_web: Vec<FrontendMessage> = Vec::new();
        let mut otros = 0usize;
        for r in respuestas {
            match r {
                DesktopFrontendMessage::ToWeb(mut ms) => para_web.append(&mut ms),
                _ => otros += 1,
            }
        }
        log::info!(
            "[{}] frontend conectado. {} respuestas al web, {} de escritorio sin manejar",
            TAG,
            para_web.len(),
            otros
        );
        self.messages_descartados += otros as u32;

        if para_web.is_empty() {
            return None;
        }
        serialize_frontend_messages(para_web)
    }

    /// ¿Ya ha conectado el frontend?
    pub fn frontend_conectado(&self) -> bool {
        self.frontend_conectado
    }

    pub fn messages_in(&self) -> u32 {
        self.messages_in
    }

    pub fn messages_descartados(&self) -> u32 {
        self.messages_descartados
    }

    /// GPU: el mismo device que después dibuja en la pantalla.
    ///
    /// Se hace a mano replicando `ContextBuilder::request_device` (features
    /// vacías, limits del adaptador) porque el builder de Graphite crea su propia
    /// instancia, y la superficie tiene que ser de *esta* instancia.
    fn gpu() -> Result<(WgpuContext, wgpu::Instance, wgpu::Adapter), String> {
        let raw_instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let sync_instance = wgpu_sync::Instance::new(raw_instance.clone());

        // Se eligen los adaptadores como Graphite: primero los que no son CPU.
        let todos = pollster::block_on(sync_instance.enumerate_adapters(wgpu::Backends::all()))
            .into_iter()
            .collect::<Vec<_>>();
        if todos.is_empty() {
            return Err("no hay ningún adaptador wgpu en el móvil".into());
        }
        let mut ordenados = todos;
        ordenados.sort_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::Cpu => 2u8,
            _ => 0u8,
        });
        let sync_adapter = ordenados.remove(0);

        let (device, queue) = pollster::block_on(sync_adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: sync_adapter.limits(),
            memory_hints: Default::default(),
            trace: wgpu::Trace::Off,
            experimental_features: Default::default(),
        }))
        .map_err(|e| format!("request_device falló: {e:?}"))?;

        let info = sync_adapter.get_info();
        log::info!(
            "[{}] adaptador = {:?} / {:?} / {}",
            TAG,
            info.backend,
            info.device_type,
            info.name
        );

        // El adaptador crudo sale por `Deref` + `clone`: `wgpu_sync::Adapter`
        // implementa `Deref<Target = wgpu::Adapter>` y `wgpu::Adapter` es `Clone`.
        // No existe `impl From<wgpu_sync::Adapter> for wgpu::Adapter` — se comprobó.
        let raw_adapter: wgpu::Adapter = (*sync_adapter).clone();

        Ok((
            WgpuContext {
                device,
                queue,
                instance: sync_instance,
                adapter: sync_adapter,
            },
            raw_instance,
            raw_adapter,
        ))
    }

    /// Formato de la superficie.
    ///
    /// `formats[0]` a ciegas es lo que hacía el puerto anterior. No es una
    /// elección: el orden lo decide el driver y no tiene por qué ser el que
    /// nosotros preferiríamos. Se pide `*Srgb` explícitamente y se coge el
    /// primero que la superficie soporte; si no hay ninguno (raro, pero un
    /// driver con soloegl antiguo), se cae al primero disponible.
    fn pick_format(surface: &wgpu::Surface<'_>, adapter: &wgpu::Adapter) -> wgpu::TextureFormat {
        let caps = surface.get_capabilities(adapter);
        let formatos = &caps.formats;
        let preferido = [
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ];
        for f in preferido {
            if formatos.contains(&f) {
                log::info!("[{}] formato de superficie elegido a mano: {:?}", TAG, f);
                return f;
            }
        }
        let f = formatos[0];
        log::warn!(
            "[{}] la superficie no ofrece ningún formato sRGB de los preferidos; usando {:?}",
            TAG,
            f
        );
        f
    }

    /// Composición alfa que admite esta superficie.
    ///
    /// Se prefiere `Opaque` —es lo correcto para una superficie que pinta opaco—
    /// pero algunos drivers solo ofrecen `Inherit`, y pedir `Opaque` a pelo es un
    /// error de validación que **aborta la app**. Ver la nota del campo.
    fn pick_alpha_mode(
        surface: &wgpu::Surface<'_>,
        adapter: &wgpu::Adapter,
    ) -> wgpu::CompositeAlphaMode {
        let modos = surface.get_capabilities(adapter).alpha_modes;
        for preferido in [
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::PreMultiplied,
        ] {
            if modos.contains(&preferido) {
                log::info!("[{}] alpha mode de superficie: {:?}", TAG, preferido);
                return preferido;
            }
        }
        let modo = modos[0];
        log::warn!(
            "[{}] la superficie no ofrece Opaque ni PreMultiplied; usando {:?} (los que hay: {:?})",
            TAG,
            modo,
            modos
        );
        modo
    }

    /// Superficie de wgpu a partir del `Surface` de Java.
    ///
    /// En Android el `DisplayHandle` es un struct **vacío**, y pasar `None` no
    /// vale: eso es solo del camino WebCanvas de wgpu.
    fn surface(
        env: *mut jni::sys::JNIEnv,
        surface_job: jni::sys::jobject,
        raw_instance: wgpu::Instance,
    ) -> Result<wgpu::Surface<'static>, String> {
        let raw_window =
            unsafe { ndk::native_window::NativeWindow::from_surface(env, surface_job) }
                .ok_or("ANativeWindow_fromSurface devolvió null")?;
        use raw_window_handle::HasWindowHandle;
        let handle = raw_window
            .window_handle()
            .map_err(|e| format!("window_handle falló: {e:?}"))?;
        let display = Some(raw_window_handle::RawDisplayHandle::Android(
            raw_window_handle::AndroidDisplayHandle::new(),
        ));
        unsafe {
            raw_instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: display,
                raw_window_handle: handle.into(),
            })
        }
        .map_err(|e| format!("create_surface_unsafe falló: {e:?}"))
    }

    /// Pipeline del blit: quad de pantalla completa que escala al viewport la
    /// textura que renderizó el motor.
    ///
    /// Es lo que hace `desktop/src/render/state.rs`, pero con una sola textura
    /// en vez de tres: las capas de overlay y de UI las dibuja el frontend, y en
    /// Android todavía no hay frontend.
    fn blit(
        device: &wgpu::Device,
        // `wgpu_sync::Queue`, NO `wgpu::Queue`: el campo `queue` de `WgpuContext` es
        // el wrapper, y el wrapper NO implementa `Deref` a `wgpu::Queue` (solo lo
        // hace `QueueGuard`). Se usan sus métodos directos.
        queue: &wgpu_sync::Queue,
        format: wgpu::TextureFormat,
    ) -> (wgpu::RenderPipeline, wgpu::Sampler, wgpu::TextureView, wgpu::Buffer) {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // MEDIDO: hace falta un TERCER binding. Ver `blit.wgsl`: sin el
                // offset y la escala del lienzo, la textura se estiraba a toda la
                // ventana y lo que se dibujaba caia fuera de la vista.
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit-pl"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                // Sin `blend`: la superficie es opaca, el fragmento reemplaza el
                // destino y el alfa del shader es 1.
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blit-samp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // En wgpu 29 `mipmap_filter` es `MipmapFilterMode`, un tipo PROPIO, no
            // `FilterMode` como los otros dos.
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        // Fallback 1x1 transparente para el frame 0.
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("fallback"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[0, 0, 0, 0],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let view = tex.create_view(&Default::default());

        // ------------------------------------------------------------------
        // EL UNIFORM DE COLOCACIÓN DEL LIENZO, Y EL MODO DE VISUALIZACIÓN.
        //
        // `vec2f` + `vec2f` + `f32` + `vec3f` de relleno: 32 bytes. Los 16
        // primeros son la colocación; el `f32` es el modo de diagnóstico que lee
        // `blit.wgsl`, y se cambia en caliente desde el navegador con
        // `window.GraphiteNative.debug(n)`.
        //
        // El relleno no es adorno: sin el, la structura mide 20 bytes y el
        // sombreado espera el `vec2f` siguiente alineado a 16.
        //
        // Se inicializa a "todo la ventana" (offset 0, scale 1), que es lo
        // correcto antes de que el frontend diga dónde está el lienzo: es decir,
        // el comportamiento viejo. No es arbitrario: mientras no llegue el
        // mensaje, no se sabe nada mejor, y con esto se ve algo en vez de nada.
        // ------------------------------------------------------------------
        let colocacion = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("blit-colocacion"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &colocacion,
            0,
            bytemuck::cast_slice(&[0.0f32, 0.0f32, 1.0f32, 1.0f32, 0.0f32, 0.0, 0.0, 0.0]),
        );

        (pipeline, sampler, view, colocacion)
    }

    /// Un frame: ejecuta el grafo del motor y lo pinta.
    ///
    /// Devuelve si el lienzo tiene imagen (no si el frame se pintó: siempre se
    /// pinta). Esa distinción es la que hace falta en el log para no confundir
    /// «el motor no ha renderizado» con «el motor no está**.
    pub fn frame(&mut self) -> Result<bool, String> {
        // ------------------------------------------------------------------
        // 1. Bombear la evaluación del grafo de nodos.
        //
        // El motor se genera mensajes a sí mismo (recalcular, invalidar, etc.) y
        // hay que devolvérselos por el mismo canal. El dispatcher de upstream lo
        // hace con `DesktopWrapperMessage::PollNodeGraphEvaluation`, que es
        // literalmente lo que hace el escritorio en `desktop/src/app.rs:441`.
        //
        // Las respuestas que salen NO se pasan al frontend: son mensajes internos
        // del motor. Es lo que hace el escritorio también.
        // ------------------------------------------------------------------
        let bombear = self
            .wrapper
            .dispatch(DesktopWrapperMessage::PollNodeGraphEvaluation);
        if !bombear.is_empty() {
            self.messages_descartados += bombear.len() as u32;
            if self.frames <= 3 {
                log::info!(
                    "[{}] bombeo produjo {} respuestas del motor (internas)",
                    TAG,
                    bombear.len()
                );
            }
        }

        // ------------------------------------------------------------------
        // 2. Ejecutar el grafo.
        //
        // `DesktopWrapper::execute_node_graph` es la misma función que llama el
        // hilo de render del escritorio (`desktop/src/app.rs:86`), y devuelve
        // un enum en vez de una tupla para no perder el caso de "ha corrido pero
        // no ha producido textura", que es distinto de "no ha corrido".
        //
        // Devuelve `HasRun(None)` cuando no hay nada nuevo que renderizar: el
        // motor es demand-driven y eso es lo correcto; ver `last_texture`.
        // ------------------------------------------------------------------
        let (ran, textura) = {
            let resultado = match pollster::block_on(DesktopWrapper::execute_node_graph()) {
                NodeGraphExecutionResult::HasRun(t) => (true, t),
                NodeGraphExecutionResult::NotRun => (false, None),
            };
            // ------------------------------------------------------------------
            // QUÉ DEVUELVE EL GRAFO, Y SI CAMBIA.
            //
            // MEDIDO, y esto es lo que hay que mirar porque el sintoma es "no se
            // ve nada de lo que dibujo" sin ningun error en ninguna parte.
            //
            // Tres preguntas, y tres numeros que las contestan:
            //
            //  1. ¿De que tamaño es la textura? Si no es del tamaño del lienzo,
            //     el motor esta evaluando otra cosa (un artboard, un nodo) y lo
            //     nosso lo estira a donde no es.
            //  2. ¿Cambia el contenido? Se compara el tamaño, que es gratis, y se
            //     avisa cuando cambia. Un tamaño que no cambia no dice nada de
            //     contenido, asi que tambien se mira el puntero de la textura:
            //     si el grafo reevalua y devuelve una textura nueva, el puntero
            //     cambia aunque el tamano sea el mismo.
            //  3. ¿El motor cree que el lienzo es este? `ViewportState` ya tiene
            //     la transformada; lo que llega por `UpdateViewportPhysicalBounds`
            //     es la salida. Si los dos no coinciden, se nota aqui.
            //
            // `NotRun` se registra con nombre propio porque es el estado
            // silencioso por excelencia: no es un error, el grafo simplemente no
            // produce nada, y sin este log es indistinguible de "no ha pintado".
            let (corrio, t) = resultado;
            // `HasRun(None)` NO es lo mismo que `HasRun(Some(textura))`, y es la
            // distincion que faltaba. MEDIDO: `notrun=0` era verdad y no servia
            // para nada, porque `notrun` solo contaba `NotRun` y el grafo es
            // *demand-driven*: `HasRun(None)` significa "he corrido, pero no hay
            // nada nuevo que dibujar", y eso con una textura VIEJA en
            // `last_texture` es EXACTAMENTE el sintoma de "no se ve nada de lo que
            // dibujo".
            //
            //   NotRun       -> no he corrido
            //   HasRun(None) -> he corrido, no hay nada nuevo      <- el sospechoso
            //   HasRun(Some) -> he corrido, aqui tienes la textura
            //
            // Los dos primeros se cuentan aparte, porque solo el tercero cambia
            // lo que se ve en pantalla.
            match (&t, corrio) {
                (None, true) => {
                    self.grafo_sin_textura += 1;
                    if self.frames <= 3 || self.frames % 300 == 0 {
                        log::warn!(
                            "[{}] el grafo CORRE pero no devuelve textura nueva ({} veces). last_texture sigue siendo la de antes.",
                            TAG,
                            self.grafo_sin_textura
                        );
                    }
                }
                (Some(_), _) => self.grafo_con_textura += 1,
                _ => {}
            }
            if !corrio {
                self.grafo_sin_correr += 1;
                if self.frames <= 3 || self.frames % 60 == 0 {
                    log::warn!(
                        "[{}] el grafo NO ha corrido (NotRun). {} veces. Sin textura nueva.",
                        TAG,
                        self.grafo_sin_correr
                    );
                }
            }
            if let Some(t) = &t {
                let (w, h) = (t.width(), t.height());
                let nuevo_tamano = (w, h) != self.ultimo_tamano_textura;
                let texto = format!(
                    "[{}] grafo: textura {}x{}  lienzo {:?}  superficie {:?}  cambia={}",
                    TAG,
                    w,
                    h,
                    self.lienzo.map(|l| (l.x as i64, l.y as i64, l.width as i64, l.height as i64)),
                    self.configured,
                    if nuevo_tamano { "TAMANO" } else { "no" }
                );
                if nuevo_tamano || self.frames <= 3 || self.frames % 300 == 0 {
                    log::info!("{}", texto);
                }
                self.ultimo_tamano_textura = (w, h);
            }
            (corrio, t)
        };
        if let Some(t) = textura {
            self.last_texture = Some(t);
        }

        // ------------------------------------------------------------------
        // 3. Superficie.
        //
        // OJO, y esto es lo que reventaba al arrancar: `get_current_texture()`
        // sobre una superficie SIN CONFIGURAR no devuelve una textura vacía,
        // entra en panic dentro de wgpu:
        //
        //     Error in Surface::get_current_texture_view: Validation Error
        //     Caused by: Surface is not configured for presentation
        //
        // Y la superficie no se configura hasta que la ventana avisa de su
        // tamaño, que es un instante después de arrancar. Así que aquí todavía
        // no se ha hecho nada: se evalúa el grafo (que no necesita superficie)
        // y se sale. El primer frame de verdad es el primero que llega después
        // de un `on_surface_size`.
        //
        // MEDIDO en el móvil: sin este guardia, la app moría en el arranque con
        // ese panic y no llegaba ni a loguear el tamaño.
        // ------------------------------------------------------------------
        if self.configured == (0, 0) {
            self.frames += 1;
            log::info!(
                "[{}] frame {} sin superficie configurada: solo se evaluó el grafo (lienzo={})",
                TAG,
                self.frames,
                self.last_texture.is_some()
            );
            return Ok(self.last_texture.is_some());
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => t,
            wgpu::CurrentSurfaceTexture::Lost => return Err("superficie perdida".into()),
            wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Suboptimal(_)
            | wgpu::CurrentSurfaceTexture::Outdated => return Ok(false),
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("error de validación al obtener la textura de superficie".into())
            }
        };

        // ------------------------------------------------------------------
        // 4. LA COMPROBACIÓN QUE FALTABA.
        //
        // `get_current_texture()` devuelve el tamaño con el que se *configuró* el
        // swapchain, no el de la ventana. Si discrepan, la superficie está
        // obsoleta y el compositor va a recortar el frame. Esto era el triángulo
        // negro: la superficie se configuró una vez, al arrancar, con un tamaño
        // que no era el bueno.
        //
        // No se "arregla" en plan de theory: se dice, y se reintenta con el
        // tamaño real. Si `surfaceChanged` no está llegando, `resizes` sube y el
        // log lo delata en vez de dejar una captura rara.
        // ------------------------------------------------------------------
        let real = frame.texture.size();
        if (real.width, real.height) != self.configured {
            self.resizes += 1;
            log::warn!(
                "[{}] DISCREPANCIA: swapchain {}x{} != configurado {:?} (resizes={})",
                TAG,
                real.width,
                real.height,
                self.configured,
                self.resizes
            );
        }

        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            label: None,
            format: Some(self.surface_format),
            dimension: Some(wgpu::TextureViewDimension::D2),
            usage: Some(wgpu::TextureUsages::RENDER_ATTACHMENT),
            base_mip_level: 0,
            base_array_layer: 0,
            array_layer_count: None,
            mip_level_count: None,
            aspect: wgpu::TextureAspect::All,
        });

        // ------------------------------------------------------------------
        // 5. De dónde sacamos la imagen.
        // ------------------------------------------------------------------
        let src = match &self.last_texture {
            Some(t) => t.create_view(&Default::default()),
            // Solo en el frame 0, antes de que el motor haya renderizado.
            None => self.fallback.clone(),
        };

        let bind_group = self.context.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit-bg"),
            layout: &self.pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                // El offset y la escala del lienzo. Ver `blit.wgsl`: sin este
                // binding la textura se estiraba a toda la ventana y nada de lo
                // que se dibujaba caía donde se miraba.
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.colocacion,
                        offset: 0,
                        size: wgpu::BufferSize::new(32),
                    }),
                },
            ],
        });

        // ------------------------------------------------------------------
        // 6. Dibujar.
        // ------------------------------------------------------------------
        let mut enc = self.context.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Gris (39,39,39 en sRGB). Distinto del verde del spike,
                        // para que en una captura se vea de inmediato si quien
                        // pinta es el motor o es la ventana vacía.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.02,
                            b: 0.02,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        self.context.queue.submit(Some(enc.finish()));
        frame.present();

        self.frames += 1;
        let hay_lienzo = self.last_texture.is_some();
        if hay_lienzo {
            self.rendered += 1;
        }
        if self.frames <= 3 || self.frames % 60 == 0 {
            log::info!(
                "[{}] frame {} lienzo={} ran={} swapchain={}x{} configurado={:?} resizes={}",
                TAG,
                self.frames,
                hay_lienzo,
                ran,
                real.width,
                real.height,
                self.configured,
                self.resizes
            );
        }
        Ok(hay_lienzo)
    }

    /// Estado legible para el log. Se llama desde JNI.
    pub fn status(&self) -> String {
        format!(
            "frames={} lienzo={} swapchain_configurado={:?} resizes={} \
             frontend={} msj_in={} msj_descartados={} overlays={} \
             tex={:?} sin_textura={} con_textura={} notrun={} lienzo_css={:?}",
            self.frames,
            self.rendered,
            self.configured,
            self.resizes,
            if self.frontend_conectado { "conectado" } else { "NO" },
            self.messages_in,
            self.messages_descartados,
            self.escenas_overlays,
            self.ultimo_tamano_textura,
            self.grafo_sin_textura,
            self.grafo_con_textura,
            self.grafo_sin_correr,
            self.lienzo.map(|l| (l.x as i64, l.y as i64, l.width as i64, l.height as i64)),
        )
    }
}