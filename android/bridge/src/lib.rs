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
        let (pipeline, sampler, fallback) = Self::blit(&context.device, &context.queue, surface_format);

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
            last_texture: None,
            report,
            frames: 0,
            rendered: 0,
            resizes: 0,
            messages_in: 0,
            messages_descartados: 0,
            frontend_conectado: false,
        })
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
            log::info!(
                "[{}] superficie reconfigured {:?} -> {}x{}",
                TAG,
                anterior,
                width,
                height
            );
        }

        // El viewport del motor va con la superficie. Si se queda atrás, el motor
        // renderiza al tamaño viejo y el compositor lo escala.
        //
        // El tamaño sale de `RenderConfig.viewport: Footprint`
        // (node-graph/libraries/application-io/src/lib.rs) y lo manda
        // `ViewportMessage::Update`.
        //
        // En el escritorio lo manda el frontend. Aquí lo mandamos nosotros al
        // arrancar, para que el lienzo tenga el tamaño correcto desde el primer
        // frame y no haya un parpadeo de 1x1. Cuando el frontend esté montado,
        // él lo mandará con las medidas reales del panel de dibujo, que es lo
        // que hay que acabar usando: el viewport del MOTOR no es la pantalla
        // entera, es el rectángulo donde va el lienzo, que en la UI real es más
        // pequeño que la pantalla porque hay paneles alrededor.
        self.wrapper.dispatch(DesktopWrapperMessage::FromWeb(Box::new(
            Message::Viewport(ViewportMessage::Update {
                x: 0.,
                y: 0.,
                width: width as f64,
                height: height as f64,
                // 1.0 = un pixel de pantalla por unidad de documento.
                scale: 1.,
            }),
        )));
        log::info!("[{}] viewport {}x{} actualizado", TAG, width, height);
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

        // Separar lo que va al frontend de lo que es una petición al escritorio.
        let mut para_web: Vec<FrontendMessage> = Vec::new();
        let mut otros = 0usize;
        for r in respuestas {
            match r {
                DesktopFrontendMessage::ToWeb(mut ms) => para_web.append(&mut ms),
                _ => otros += 1,
            }
        }

        if otros > 0 {
            log::info!(
                "[{}] {} DesktopFrontendMessage de escritorio SIN manejar todavia \
                 (dialogos de fichero, menu, portapapeles, persistencia)",
                TAG,
                otros
            );
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
    /// En el escritorio esto lo dispara el `on_context_created` de CEF. Aquí lo
    /// llama el shim JS del WebView.
    ///
    /// Lo que hace falta en este punto es el estado de la aplicación:
    /// preferencias, documentos del último uso y el tamaño de la ventana. En el
    /// escritorio los carga `desktop/src/app.rs`; en Android todavía no hay
    /// persistencia, así que solo se le dice al frontend las medidas.
    pub fn on_initialized(&mut self, width: u32, height: u32) {
        log::info!("[{}] el frontend pide conexión ({}x{})", TAG, width, height);

        // El frontend necesita saber cuánto tiene de ventana para maquetar sus
        // paneles. Sin esto se maqueta a 0 y se ve la UI pero con las
        // proporciones raras.
        let respuestas = self.wrapper.dispatch(DesktopWrapperMessage::UpdateMaximized {
            maximized: true,
        });
        let _ = respuestas;
        self.wrapper.dispatch(DesktopWrapperMessage::UpdateFullscreen { fullscreen: true });

        // Sin persistencia todavia: se avisara al frontend con un estado vacio
        // en cuanto se implemente. Por ahora solo se registra que se ha conectado
        // y cuantas respuestas produjo el arranque.
        let r = self.wrapper.dispatch(DesktopWrapperMessage::Wake);
        log::info!(
            "[{}] frontend conectado. Wake produjo {} respuestas",
            TAG,
            r.len()
        );
        self.frontend_conectado = true;
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
    ) -> (wgpu::RenderPipeline, wgpu::Sampler, wgpu::TextureView) {
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
        (pipeline, sampler, view)
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
        let (ran, textura) =
            match pollster::block_on(DesktopWrapper::execute_node_graph()) {
                NodeGraphExecutionResult::HasRun(t) => (true, t),
                NodeGraphExecutionResult::NotRun => (false, None),
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
             frontend={} msj_in={} msj_descartados={}",
            self.frames,
            self.rendered,
            self.configured,
            self.resizes,
            if self.frontend_conectado { "conectado" } else { "NO" },
            self.messages_in,
            self.messages_descartados,
        )
    }
}