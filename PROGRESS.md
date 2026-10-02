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

## 2. El bug del triángulo negro

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

## 3. La UI: por qué todavía no hay

La capa de UI falta, y es la parte grande. La app actual es **solo el lienzo**.

El frontend de Graphite es una aplicación Svelte de 15.101 líneas. Hay dos caminos:

| | WebView (frontend Svelte dentro de un `WebView`) | Reescribir en Kotlin/Compose |
|---|---|---|
| Volumen | ~1-2k líneas de puente | 15k+ líneas, ~38 lotes |
| Overlays del lienzo | Vienen gratis (los dibuja el frontend) | Hay que reconstruirlos |
| Gestos táctiles | Vienen gratis (`PointerMessage` ya existe) | Hay que traducirlos |
| Sensación nativa | No | Sí |
| Riesgo | SVG en WebView sobre un móvil de gama media | El tiempo |

El camino de WebView es viable porque, en `desktop/`, **solo `ui/src/ipc.rs` es
específico de CEF**. El resto (`wrapper/src/message_dispatcher.rs`,
`intercept_editor_message.rs`, `intercept_frontend_message.rs`) es Rust agnóstico
de transporte: lo que hay que reescribir es el transporte, no la lógica.

Antes de nada de eso, el render path tiene que estar limpio. Una UI sobre un
lienzo que dibuja un triángulo negro no sirve para medir la UI.

---

## 4. Compilar

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

## 5. Estado

- [x] Motor compilado para aarch64 y verificado *dentro* del `.so` (el CI
      comprueba símbolos y tamaño, porque un `.so` sin motor pesa 4,95 MB y da
      verde igual).
- [x] `SurfaceView` → `ANativeWindow` → superficie wgpu → blit.
- [x] `Editor::new` + documento + `run_node_graph()`.
- [ ] **Verificar en el móvil que el triángulo ha desaparecido** (log, no foto).
- [ ] Tamaño de pantalla correcto tras el `run_node_graph` (no 1×1).
- [ ] Gestos: traducir eventos de toque a `PointerMessage`.
- [ ] Capa de UI (WebView o nativa).
- [ ] Overlay del lienzo.