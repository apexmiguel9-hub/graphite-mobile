# Graphite en Android

Port del motor de [Graphite](https://github.com/GraphiteEditor/Graphite) a móvil.
El motor (Rust + wgpu + Vello) ya funciona en Android sin tocarlo; lo que aquí se
construye es el **puente** y la **capa de app**.

**Regla del proyecto:** nada se da por bueno sin un hecho medido. Un verde en CI
que no se haya comprobado en un móvil no cuenta.

---

## 1. Por qué este repo es un fork intacto

El repo `apexmiguel9-hub/graphite-android` (el intento anterior) modificaba el
motor para Android **borrando 264 ficheros de upstream**: `desktop/` entero,
`frontend/` entero y varios `tools/`.

Eso no es "código sucio", es una mala forma, y cuesta caro de tres formas:

1. Cada `git merge upstream/master` es una pelea: upstream sigue tocando
   `frontend/` y `desktop/`, y los borres una y otra vez.
2. Tiraste `desktop/wrapper/`, que es justamente el contrato de mensajes
   frontend↔motor que hace falta para cualquier UI.
3. `.gitignore` se **reemplazó** en vez de ampliarse, perdiendo `branding/`,
   `*.spv`, `third-party-licenses.txt*`… `branding/` es el tarball de iconos que
   Graphite descarga de una URL externa (`about.toml`), así que acabar
   commiteando unos 200 SVG era cuestión de tiempo.

Aquí, en cambio:

| | |
|---|---|
| Ficheros de upstream borrados | **0** |
| Líneas de upstream modificadas | **1** (`Cargo.toml`, un miembro nuevo) |
| Commit en el motor | 2 ficheros, ~25 líneas: `Platform::Android` |

`desktop/` sigue en el repo, pero **CEF nunca se compila**: `cargo build -p
graphite-android-bridge` solo construye el grafo de ese paquete, y CEF requiere
descargar y compilar Chromium. No hace falta borrar nada para no pagar ese
coste.

`Platform::Android` es un cambio pequeño y defendible — antes se emulaba con
`Desktop` + `Host::Linux`, que solo mentía. De hecho merece ser PR upstream.

---

## 2. Estado MEDIDO en el móvil

Motorola G56, PowerVR B-Series BXM-8-256, vía **Vulkan**. Medido por logcat, no
por foto.

| hecho | valor |
|---|---|
| adaptador | `Vulkan / IntegratedGpu / PowerVR B-Series BXM-8-256` |
| formato de superficie | `Rgba8UnormSrgb` (elegido, no el primero de la lista) |
| alpha mode | `Inherit` — el único que ofrece ese driver |
| `Editor::new` | 8,0 s |
| documento | 21 respuestas, realimentadas al motor |
| tamaño de superficie | `surfaceChanged -> 1080x2400` |
| reconfiguración | `superficie reconfigured (0, 0) -> 1080x2400` |
| viewport del motor | `viewport 1080x2400 actualizado` |
| **frames** | **7.800+ seguidos a ~60 fps, sin parar** |
| **`resizes`** | **0** — el swapchain cuadró siempre con lo configurado |
| pánicos | 0 |
| **triángulo negro** | **desaparecido** |

**Resultado: gris, y `resizes = 0`.** Que es exactamente la casilla buena de la
tabla de abajo: era el alfa premultiplicado, y el tamaño ya estaba bien.

El lienzo es gris porque el documento está vacío: `run_node_graph()` renderiza
pero no hay nada que dibujar, y el operador `over` deja ver el fondo. Es lo
correcto. El escritorio tampoco pinta nada hasta que se carga el arte de demo.

---

## 3. El bug del triángulo negro

**Síntoma:** un triángulo negro en mitad de la pantalla; el resto del lienzo se
veía bien.

**Cómo se llega al diagnóstico.** El intento anteriorIAF同期 siguiendo dos hipótesis
y las dos fallaron:

1. *El shader perforaba la capa del `SurfaceView`* → se forzaron alfa 1 en el
   blit. El triángulo siguió ahí. **Falsa.**
2. *La superficie no cubría la pantalla* → se puso la limpieza en MAGENTA para
   distinguirla del negro. Noorló nada. **No concluyente.**

Pero el razonamiento dejó una contradicción muy informativa, escrita en el propio
commit:

> El gris (39,39,39) es exactamente el color de limpieza, y se ve en casi toda la
> pantalla. Pero el blit no tiene blending, así que el quad **reemplaza** el
> destino en todo su viewport. Si el quad cubriese la pantalla, el gris no se vería
> en ningún sitio.

Esa contradicción es la firma de un **swapchain configurado con una extensión que
no es la del `ANativeWindow`**: el quad dibuja en todo *su* viewport, y lo que hay
fuera es memoria del buffer que nadie ha limpiado.

**Causa.** Dos defectos en el mismo sitio:

1. `wgpu::Surface::configure()` se llamaba **una sola vez**, dentro de `boot()`,
   con el tamaño que le pasaba Kotlin.
2. En Kotlin, ese tamaño venía de `surfaceCreated`, que en Android se dispara
   **antes del layout** — la vista todavía mide 0×0. Y el `surfaceChanged`, que
   es donde Android avisa del tamaño bueno, estaba implementado como `= Unit`.

O sea: la superficie se quedaba con una extensión obsoleta para siempre, y el
compositor recortaba el frame. El triángulo no era un artefacto del render ni del
shader.

**Por qué el diagnóstico con MAGENTA no podía servir.** El color de limpieza es
irrelevante cuando el quad nunca tocó esa región: da igual lo que limpies, lo que
se ve ahí no es tu limpieza. Por eso las dos hipótesis anteriores no se podían
descartar ni confirmar mirando el shader.

**El arreglo**, tal y como está implementado:

- `Engine::boot()` **no** toca la superficie. Arrancar el motor y dimensionar la
  ventana son dos cosas independientes, y acoplarlas es lo que congeló el tamaño.
- `Engine::on_surface_size(w, h)` reconfigura la superficie **y** el viewport del
  motor. Se llama desde `surfaceCreated` **y** desde `surfaceChanged`. Ignora los
  tamaños cero.
- `Engine::frame()` **mide** el tamaño real del swapchain cada frame y lo compara
  con lo configurado. Si discrepan, lo dice en el log y cuenta un `resizes`. Así,
  si algún día `surfaceChanged` deja de llegar, el log lo delata en vez de dejar
  una captura rara.

**De dónde vino el bug: el spike.** El port anterior se apoyó en
`wgpu-android-spike`, un repositorio de pruebas para ver si Vello y wgpu
funcionaban en Android, y heredó su gestión de la superficie. El spike tiene,
literalmente, el mismo defecto:

```kotlin
// wgpu-android-spike/packaging/.../MainActivity.kt:77
override fun surfaceChanged(h: SurfaceHolder, f: Int, w: Int, ht: Int) = Unit

// spike/.../MainActivity.kt:127-129
private fun startRender(surface: Surface, width: Int, height: Int) {
    val w = if (width > 0) width else 1080   // número mágico
    val h = if (height > 0) height else 2400 // número mágico
}
```

`surfaceCreated` siempre entrega 0×0, así que el spike **siempre** caía al
fallback. No se notó porque el móvil de prueba era de 1080×2400 y el número
casaba por casualidad con el dispositivo. Ese es el modo de fallo completo: no
era un descuido del spike, era un spike que parecía funcionar por suerte y
enseñó el error al código que se apoyó en él.

Aquí no hay números mágicos: el tamaño llega, se comprueba, y si es 0 se
ignora en vez de inventarse uno.

### Cómo se comprueba en el móvil

```bash
adb logcat -s GRAPHITE | grep -E 'surfaceChanged|reconfig|DISCREPANCIA|viewport'
```

- **Correcto:** aparece `surfaceChanged -> 1080x2400`, `superficie reconfigured
  (0, 0) -> 1080x2400`, `viewport 1080x2400 actualizado`, y **cero**
  `DISCREPANCIA`.

El **color** de lo que se vea y el contador `resizes` separan las hipótesis sin
ambigüedad:

| | `resizes = 0` | `resizes > 0` |
|---|---|---|
| **gris (39,39,39)** | Arreglado: era el alfa premultiplicado | Arreglado, y además el tamaño estaba mal |
| **negro** | El tamaño estaba bien; el problema es otro (UV o viewport) | Problema doble: tamaño **y** shader |

Ese es el criterio. No "ya no sale en la foto", sino **el número y el color en el
log**.

### Por qué el alfa era el bug (y por qué el arreglo anterior no lo arreglaba)

Vello compone con alfa **premultiplicada**: donde el motor no dibujó, la textura
trae `rgb = 0` **y** `a = 0`.

- El shader original hacía `if (a <= 0.0001) { return vec4f(0.0); }` → escribía
  negro en la superficie opaca.
- El "arreglo" pasó a `return vec4f(c.rgb, 1.0)` → **sigue escribiendo negro
  opaco**, porque `c.rgb` es 0 ahí. Solo le puso un alfa de 1 encima.

Los dos pintan negro. El razonamiento que justificaba el segundo ("la superficie
es opaca, alfa 1 y el color tal cual") está mal desde la raíz: se despreciba la
transparencia de la fuente en vez de resolverla.

Lo correcto es componer la fuente premultiplicada **sobre un fondo opaco**, que es
el operador `over`:

```wgsl
let fondo = vec4f(0.02, 0.02, 0.02, 1.0);
let rgb = c.rgb + fondo.rgb * (1.0 - c.a);
return vec4f(rgb, 1.0);
```

Así la transparencia revela el gris del lienzo en vez de convertirse en negro.

Nótese que el diagnóstico con MAGENTA no podía detectar este bug: el shader nunca
dejó de escribir, así que el color de limpieza era irrelevante. Los tres fallos
—anclaje del tamaño, `surfaceChanged` vacío y alfa premultiplicada— se
presentaban a la vez y tapaban el uno al otro. Por eso dos hipótesis anteriores
dieron falso.

---

## 4. La UI: el mensaje que sale del motor se perdía

### El síntoma

Pantalla casi negra, sin un error en el log. El frontend cargaba, el wasm
arrancaba, y el motor contestaba:

```
js -> nativo: 28 b64 entrada, 328072 b64 salida
```

328 KB de respuesta.  Y no: ese número era el tamaño de lo que
Rust **iba a** devolver, no de lo que llegaba al frontend.

### Cómo se mide, sin gastar un ciclo de CI

El `WebView` publica un socket de DevTools:

```
/proc/net/unix -> webview_devtools_remote_<pid>
```

Con `adb forward` se puede ejecutar JavaScript en la página **real del móvil**.
Eso convierte «la pantalla está negra» en «aquí está el DOM, y aquí está el
contador». Está en `android/scripts/cdp.py` (cliente WebSocket propio, sin
dependencias) y es la herramienta de diagnóstico de este proyecto.

Lo que salió:

| Medición | Valor | Qué significa |
|---|---|---|
| `document.body.children` | `LINK`, `SCRIPT`, `DIV` | Svelte monta |
| `.main-window` | 443×984, `opacity: 1` | La interfaz tiene tamaño |
| `.workspace.children` | **0** | Los paneles no llegan |
| `receiveNativeMessage` (contador) | **0** | No entra ni un mensaje |
| `console.error` | ninguno | Ni una queja |

Un `.main-window` con barra de título, barra de estado y el icono de pantalla
completa dibujado, y todos los paneles vacíos. Ese es el frontend funcionando
perfectamente y sin que le llegue nada.

### La causa

`frontend/wrapper/src/native_communication.rs`:

```rust
pub fn send_message_to_cef(message: String) {
    let array = Uint8Array::from(message.as_bytes());
    let buffer = array.buffer();
    func.call1(&JsValue::NULL, &JsValue::from(buffer)).expect("Function call failed");
}
```

**Ignora el valor de retorno.** El wasm manda y se olvida: la respuesta vuelve
por otro camino, `window.receiveNativeMessage(<ArrayBuffer>)`, que en el
escritorio llama CEF con `execute_java_script`.

El puente de Android devolvía la respuesta como valor de retorno de JNI. Datos
que salían de Rust, se empacaban en base64, se devolvían… y se descartaban en la
línea siguiente. Sin excepción, sin aviso, sin rastro: el mejor fallo posible,
porque todo lo que se puede medir por logcat daba verde.

### Lo que se cambió

Los dos caminos pasan a ser separados, como en upstream:

```
wasm --sendNativeMessage(ArrayBuffer)-->  bridge.js
     --@JavascriptInterface--------------->  onMessage
     --nativeMessage (JNI)---------------->  Rust: dispatch -> COLA
     <-- null (JNI)                        el wasm lo IGNORA

Rust --nativeDrain (JNI)--> Kotlin --evaluateJavascript-->
     bridge.js: receiveNativeMessage(ArrayBuffer) --> wasm
```

- `nativeMessage` y `nativeInitialized` devuelven `null` y **encolan** la
  respuesta.
- `nativeDrain` saca **una** por llamada. Kotlin drena en bucle tras cada mensaje
  del frontend, y también en el bucle de render, que es donde llegan los que el
  motor produce solo.
- Entregas grandes troceadas a 128 KB. No es un fallo medido: es no depender de
  un límite de `evaluateJavascript` que no se ha medido. En el caso normal no
  trocea nunca, porque los mensajes del frontend miden 28-56 bytes.

`webUI.bombear()` serializa con un `synchronized` para que el hilo del
JavaScript y el de render no intercalen entregas: un payload partido por la
mitad es un JSON que no se puede parsear.

### Una idea que era falsa

Antes de esto se llegó a la conclusión de que «la UI de Graphite no se dibuja en
el DOM, el frontend manda escenas de Vello y hay que componerlas en Rust». Es
**falso**, y está medido: la interfaz la pinta el DOM. `desktop/ui/` recibe los
píxeles del DOM en `on_paint` y los sube a una textura.

Lo que sí son escenas de Vello es `UpdateOverlays`: los handles, las anclas y el
rectángulo de selección **del lienzo**. El escritorio las rasteriza por GPU
(`render/state.rs:225`) y las compone con el grafo de nodos. Aquí se cuentan y se
anotan en el log (`overlays=N` en `status()`), pero no se rasterizan: para
pintarlas hay que saber primero dónde está el lienzo en pantalla, que es lo
siguiente.

---

## 5. La primera versión con la UI, y lo que faltaba

La interfaz apareció y ya se podía usar. Lo que salió en los primeros cinco
minutos, todo medido en el móvil.

### 5.1 El cierre al volver del segundo plano

**Síntoma:** minimizar y reabrir. A veces el lienzo aparecía y luego, a los
pocos segundos, la app se cerraba sola.

**Cómo se mide un crash nativo en Android:** `adb logcat -b crash`, no
`-s GRAPHITE`. El tombstone va a otro buffer y fuera del tag propio.

```
signal 6 (SIGABRT), en el hilo principal
#13 Java_dev_graphite_mobile_MainActivityKt_nativeBoot+584
#19 dev.graphite.mobile.MainActivity$callback$1.surfaceCreated+0
#22 android.view.SurfaceView.updateSurface
#23 android.view.SurfaceView.setWindowStopped
#24 android.view.SurfaceView.surfaceCreated
```

El-frame lo dice: no es una ventana liberada, es **`nativeBoot` llamado dos
veces**. Android vuelve a disparar `surfaceCreated` al reanudar, y arrancar el
motor otra vez machaca el primero — al escribir el segundo sobre el primero se
destruía su `DesktopWrapper`, con el `Editor` y el `Device` de wgpu dentro, en el
hilo principal y en mitad de un frame.

**El arreglo:** el motor se arranca **una vez**. A partir de ahí, `surfaceCreated`
solo rehace la superficie (`Engine::reenganchar_superficie`). Hace falta porque
`wgpu::Surface` envuelve la `ANativeWindow` al crearse y no hay forma de
cambiarle la ventana después; pero la `Surface` se puede reemplazar sin tocar el
`Device`, el `Queue` ni el `Editor`, porque la superficie nueva usa la misma
instancia.

Las dos cosas que hay que tener juntas:

- La superficie nueva **no** puede conservar el tamaño viejo. Se marca como «sin
  configurar» y espera a `surfaceChanged`. Si `surfaceChanged` no llegara, el
  siguiente frame presentaría a una superficie sin configurar, y los errores de
  wgpu son fatales.
- El motor no se vuelve a sacar del `Mutex` ni a meter. Un `take()` ahí lo dejaría
  en `None` y el siguiente mensaje del frontend se encontraría sin motor.

### 5.2 La UI llega a las barras del sistema

**Síntoma:** la barra de título de Graphite se dibujaba debajo de las
notificaciones, y el panel de la derecha quedaba cortado por la barra de
navegación.

**No es un error de colocación: es `targetSdk = 36`.** Desde Android 15 la
ventana va de edge-to-edge **obligatoriamente**. `setDecorFitsSystemWindows` está
deprecado y el framework lo ignora, así que con el `WebView` a `MATCH_PARENT` su
CSS llega al borde de la pantalla.

Hay dos maneras de "arreglarlo" y solo una es la buena:

| | | |
|---|---|---|
| Bajar la escala un 10% | `UpdateUIScale` escala widgets y fuentes, **no mueve la barra de título** | Se seguiría solapando, y se agranda todo lo que hay que tocar |
| Respetar los insets | Lo que hace cualquier app Android: el contenido va entre las dos barras | Correcto |

Se aplica padding al `FrameLayout` raíz, no a las vistas: los dos hijos (la
`SurfaceView` del motor y el WebView) son `MATCH_PARENT`, así que los dos empiezan
debajo de la barra de estado y acaban antes de la de navegación. Si solo se
corrigiera el WebView, el motor seguiría pintando debajo de la barra y se vería el
borde, y además el hueco del lienzo dejaría de coincidir con la `SurfaceView`.

`systemBars()` y no `systemGestures()`: la barra de navegación del G56 es de
botones y cuenta como barra del sistema; con `systemGestures()` el margen de abajo
sale 0.

### 5.3 Los gestos: quién los maneja

**La UI web. Enteramente. Rust no ve el dedo ni una vez.**

| Quién | Qué hace |
|---|---|
| Chromium (`WebView`) | Convierte el toque en eventos `pointer*` del DOM |
| `frontend/src/managers/input.ts:42-44` | Los escucha **en `window`** |
| `frontend/src/utility-functions/input.ts:137,169` | Los traduce a `editor.onMouseMove/onMouseDown(x, y, buttons, mods)` |
| wasm → JNI | `EditorMessage::Input(...)` |

Medido, sin recompilar: se synthesize un arrastre real por DevTools
(`Input.dispatchTouchEvent`) y lo que llega al DOM es exactamente lo que
produciría un ratón:

```
pointerdown  target=viewport-transparent  dentroDeViewport=true  buttons=1  button=0
pointermove  dentroDeViewport=true  buttons=1   (x5)
pointerup    dentroDeViewport=true  button=0
```

Y `elementFromPoint` en el centro del lienzo devuelve `DIV.viewport-transparent`,
que **está dentro de `[data-viewport]`**, así que el filtro `isTargetingCanvas` de
`onPointerDown` pasa.

**Los gestos llegan enteros.** Por eso los menús funcionan, y por eso un arrastre
crea la layer pero no dibuja: el comando llega al motor y lo ejecuta, lo que falta
es el marco del lienzo. Eso es `UpdateViewportPhysicalBounds`, que hoy se
descarta, y es lo siguiente.

Los 24 `msj_descartados` se cuentan por tipo ahora (`nombre_de` en `lib.rs`):
«cuántos» no decía nada, había que ver «cuáles».

### 5.6 Se crea la layer pero no se ve nada: el blit no sabía dónde estaba el lienzo

El bug más caro del port, y el que más se resistió a dejar una pista.

**Síntoma:** se arrastra en el lienzo, la pestaña del documento pasa a
`Untitled Document*`, el relleno del color cambia a rojo — o sea, **el comando
llega al motor y la herramienta actúa**— pero en el lienzo no aparece nada.
Lapiz, rectángulo, texto: nada. Y lo más desconcertante, la interfaz entera
funciona: menús, paneles, pestañas, reglas.

**La pista que lo resolvió:** los **tres modos de render fallan igual** (SVG,
pixel outline, normal). Si fuera un problema de pintado, uno funcionaría. Que
fallen los tres dice que no es pintado: es **geometría**.

**Dónde estaba.** `UpdateViewportPhysicalBounds` llegaba y se descartaba:

```
DESCARTADO x2: UpdateViewportPhysicalBounds
```

Ese mensaje es el que le dice a la plataforma dónde está el lienzo dentro de la
ventana. Sin él, el blit estiraba la textura del motor a toda la superficie:

```rust
let src = match &self.last_texture { Some(t) => t.create_view(...), ... };
// quad a pantalla completa, uv de 0 a 1
```

El lienzo ocupa una región —alrededor del 60% del ancho en horizontal— y el
resto es interfaz. Estirarlo todo hacía que el lienzo se dibujara desplazado y
escalado. Lo que se dibuja cae en coordenadas que el motor no considera
visibles, y el blit lo manda a otra parte: **la capa existe, se evalúa
(`lienzo=146582`) y se pinta donde no se mira.**

El escritorio sí lo placement. `desktop/src/app.rs:285`:

```rust
let viewport_offset_x = x / window_size.width as f64;
render_state.set_viewport_offset([viewport_offset_x as f32, viewport_offset_y as f32]);
let viewport_scale_x = if width != 0. { window_size.width as f64 / width } else { 1. };
render_state.set_viewport_scale([viewport_scale_x as f32, viewport_scale_y as f32]);
```

**Las unidades ya son físicas, y eso no es casualidad.** Quien emite el mensaje
es el editor, no el frontend, y las convierte él:

```rust
// editor/src/messages/viewport/viewport_message_handler.rs:42
let physical_bounds = self.bounds().to_physical();
responses.add(FrontendMessage::UpdateViewportPhysicalBounds { x, y, width, height });
```

El frontend manda píxeles CSS en `ViewportMessage::Update`
(`frontend/src/utility-functions/viewports.ts:53`) y el editor los multiplica por
la escala que le dio. Por eso aquí no hay que convertir nada y la aritmética de
upstream aplica tal cual. Es lo que hace `blit.wgsl` ahora:

```wgsl
let coorde = (in.uv - colocacion.offset) * colocacion.scale;
if (coorde.x < 0.0 || coorde.x > 1.0 || coorde.y < 0.0 || coorde.y > 1.0) {
    return fondo;   // fuera del lienzo
}
let c = textureSample(src, samp, coorde);
```

**El margen se pinta con el fondo del motor, no con negro.** Antes no se notaba
porque la textura tapaba toda la ventana. Ahora que la textura se coloca donde
toca, el margen es visible, y negro ahí es exactamente el triángulo negro que
este mismo shader arregló una vez. No se puede volver a traer.

**Y el offset tiene que recalcularse en cada giro**, no solo cuando llega el
mensaje: la colocación depende del tamaño de la superficie, que cambia al girar.

### 5.7 El notch: `displayCutout()` no es `systemBars()`

MEDIDO, con una foto del móvil: el **Path Tool** y los círculos de color quedan
debajo de la cámara.

Con solo `systemBars()`, los insets salían:

```
Insets{left=0, top=115, right=117, bottom=0}
```

`left = 0` en horizontal, con la cámara claramente ahí. La causa es que el recorte
de pantalla es un **tipo de inset aparte**, no parte de las barras del sistema. Las
barras son barras; el notch es un agujero en la pantalla. Pedir solo las barras da
0 donde hay un recorte.

```kotlin
insets.getInsets(
    WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
)
```

La `|` y no una suma, porque los dos tipos se solapan —la barra de estado también
es zona de cutout— y sumarlos contaría el margen dos veces.

Se loguea también `displayCutout.boundingRect()`, que es lo que hay que
**comprobar**: si con este cambio el Path Tool sigue debajo de la cámara, el log
dirá si Android no lo está reportando o si hay que ir a por él de otra forma. Antes
esto era una suposición.

---

### 5.9 Compilar Rust en local: sí se puede, con el compilador de Debian

Costó, pero compilar en local convierte un ciclo de 12 minutos en segundos. Y
todo el camino fue culpa de usar el compilador equivocado para el sistema
equivocado.

El entorno es un PRoot de **Debian 13 con glibc**, dentro de Termux. El
`rustc` que estaba en el PATH era el de Termux, que compila para **Bionic**:

| | |
|---|---|
| rustup + clang de Termux | ❌ `libgcc_s`, luego 4 símbolos de glibc, luego el segmento TLS desalineado |
| Shim propio de 4 símbolos en C | 🟡 enlazaba, pero Bionic abortaba el binario al ejecutarlo |
| `apt install rustc` (1.85.1) | ❌ MSRV: el workspace pide **1.98** |
| **`~/.cargo/bin/cargo` + `/usr/bin/gcc` de Debian** | ✅ **enlaza con glibc y ejecuta** |

La clave es que el toolchain de rustup **sí** es el bueno (1.99.0, el que pide el
proyecto) y es `aarch64-unknown-linux-gnu`. Lo que estaba mal era el
**enlazador**: `cc` resolvía al clang de Termux, que apunta a Android. Con el gcc
de Debian, dentro del rootfs de Debian, enlaza contra glibc y el binario arranca.

Queda un obstáculo: `ndk-sys` tiene un `compile_error!` deliberado fuera de
Android, y sin él no se comprueba el puente. Se resuelve parcheando una copia en
`/tmp` y usando `--config patch.crates-io...`, porque `cargo check` **no enlaza** y
las referencias a `ANativeWindow_fromSurface` nunca se resuelven:

```bash
# El camino rápido. NO compila el .so de verdad: eso sigue siendo CI, que tiene
# el NDK. Lo que da es comprobación de tipos de TODO el grafo, en segundos.
cp -r ~/.cargo/registry/src/*/ndk-sys-* /tmp/ndk-sys && chmod -R u+w /tmp/ndk-sys
# quitar el compile_error! de /tmp/ndk-sys/src/lib.rs

env -u RUSTUP_HOME -u CARGO_HOME \
  CC=/usr/bin/gcc CXX=/usr/bin/g++ \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=/usr/bin/gcc \
  CARGO_TARGET_DIR=/tmp/target-deb2 \
  PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  ~/.cargo/bin/cargo check -p graphite-android-bridge --message-format short \
    --config 'patch.crates-io.ndk-sys.path="/tmp/ndk-sys"'
```

**Y cuándo NO compensa**, que es la parte que hay que decir y no omitir:

- **Añadir o quitar una dependencia invalida el grafo entero.** Al meter
  `tracing-subscriber`, el grafo tardó **48 minutos** recompilando `graphene_std`
  y compañía. CI tarda diez. Añadir dependencias: CI gana.
- **Cambiar código sí**: entonces son ~20 segundos, porque no se toca nada del
  grafo. Ahí es donde vale.
- Ojo con dejar procesos vivos: un `cargo` abortado deja el lock del directorio de
  compilación tomado y el siguiente se queda en `Blocking waiting for file lock`
  sin explicar nada. Pasa.

**Lo que esto NO comprueba**, y es lo importante: compila para
`aarch64-unknown-linux-gnu`, no para `aarch64-linux-android`. Las ramas
`cfg(target_os = "android")` de upstream —el brazo `Platform::Android`, el de
`AppWindowPlatform::Android`— **no se compilan aquí**. Ese hueco lo cubre CI y
solo CI. Todo lo demás es código nuevo y sí se comprueba.

## 6. La UI: por qué WebView y no reescribir

La capa de UI es la parte grande. El frontend de Graphite es una aplicación
Svelte de 15.101 líneas. Hay dos caminos:

| | WebView (frontend Svelte dentro de un `WebView`) | Reescribir en Kotlin/Compose |
|---|---|---|
| Volumen | ~1-2k líneas de puente | 15k+ líneas, ~38 lotes |
| Overlays del lienzo | Escenas de Vello ya disponibles | Hay que reconstruirlos |
| Gestos táctiles | Vienen gratis (`PointerMessage` ya existe) | Hay que traducirlos |
| Sensación nativa | No | Sí |
| Riesgo | SVG en WebView sobre un móvil de gama media | El tiempo |

El camino de WebView es viable porque, en `desktop/`, **solo `ui/src/ipc.rs` es
específico de CEF**. El resto (`wrapper/src/message_dispatcher.rs`,
`intercept_editor_message.rs`, `intercept_frontend_message.rs`) es Rust agnóstico
de transporte: lo que hay que reescribir es el transporte, no la lógica.

Una medición que salió de este camino y que se habría tardado en ver de otra
forma: el `.so` del puente **no lleva el motor**. El motor va aparte, en
`libgraphite_editor.so`, porque el frontend se compila con el feature `native` y
solo necesita el *dispatcher* (`EditorWrapper.create` no crea ningún `Editor`,
solo cablea el callback: `editor_wrapper.rs:87`).

---

## 7. Herramientas de diagnóstico

Todo lo de este proyecto se ha medido en el móvil. Estas son las herramientas,
en orden de utilidad:

| | Qué da |
|---|---|
| `adb exec-out screencap -p` | Lo que se ve de verdad. Un PNG. |
| `android/scripts/cdp.py` | Ejecuta JS en la página real del WebView por DevTools. El DOM, los contadores, los errores de consola, capturas. |
| `adb logcat -d -s GRAPHITE` | El log propio y la consola del WebView (la envuelve `onConsoleMessage`). |
| `adb logcat -d \| grep -i 'AndroidRuntime\|UnsatisfiedLink'` | Las excepciones de Java, que `onConsoleMessage` no ve. |

Para conectar el puerto de DevTools, que hay que rehacer en cada reinicio porque
cambia el PID:

```bash
adb forward tcp:9222 localabstract:webview_devtools_remote_$(adb shell pidof dev.graphite.mobile)
python3 android/scripts/cdp.py --cuerpo    # qué hay en el DOM
python3 android/scripts/cdp.py --shot out.png
```

---

## 8. Compilar

```bash
# El motor, para arm64-v8a (tarda ~10 min la primera vez)
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs \
  build --release -p graphite-android-bridge

# El APK
cd android && ./gradlew assembleDebug
```

En CI los dos van separados, a propósito: tocar la UI no debe pagar los 10
minutos del motor.

---

## 9. Estado

- [x] Motor compilado para aarch64 y verificado *dentro* del `.so` (el CI
      comprueba símbolos y tamaño, porque un `.so` sin motor pesa 4,95 MB y da
      verde igual).
- [x] `SurfaceView` → `ANativeWindow` → superficie wgpu → blit.
- [x] `Editor::new` + documento + `run_node_graph()`.
- [x] **Triángulo negro desaparecido**, con `resizes = 0` y 7.800 frames a 60 fps.
- [x] Tamaño de superficie correcto y el viewport del motor siguiendo al tamaño.
- [x] **Los cinco símbolos JNI resuelven**, y el check deriva el nombre del
      fichero de verdad (`android/scripts/check-jni-symbols.py`).
- [x] **El frontend carga y habla con el motor.** Frontend Svelte compilado en
      modo nativo: wasm de 271 KB, bundle 444 KB, CSS 99 KB.
- [x] **La respuesta del motor llega al frontend** (cola + `nativeDrain`). Ver §4.
- [ ] Ver la interfaz con contenido real. Lo siguiente.
- [ ] Gestos: traducir eventos de toque a `PointerMessage`.
- [ ] Overlay del lienzo: rasterizar `UpdateOverlays` (escenas de Vello).

### Los cuatro fallos que solo aparecieron midiendo en el móvil

Ninguno se podía ver leyendo el código; los cuatro son **preguntarle cosas a
wgpu sin preguntarle antes al dispositivo**, y los errores de wgpu son fatales
por defecto, así que cada uno tumbaba la app en el arranque:

| | Error que daba wgpu | Por qué |
|---|---|---|
| 1 | `Surface is not configured for presentation` | se pedía textura antes de que existiera tamaño |
| 2 | `Requested alpha mode Opaque is not in ... [Inherit]` | ese driver solo admite `Inherit` |
| 3 | `Failed to wait for GPU to come idle before reconfiguring` | había trabajo de render en vuelo |
| 4 | — | el alfa premultiplicada, que no es de wgpu pero es del mismo género |

Los tres primeros se arreglan preguntando a `surface.get_capabilities()` y
esperando la GPU. Nada de eso es adivinar: ya está medido.