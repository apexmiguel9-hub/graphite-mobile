// Blit: lleva la textura que ha renderizado el motor a la superficie del móvil.
//
// Es un quad de pantalla completa generado con `vertex_index` (sin vertex
// buffer). Dos triangulos que cubren el clip-space entero [-1, 1].
//
// Sobre el alfa: la fuente viene con alfa PREMULTIPLICADA (así compone Vello) y
// el destino es una superficie opaca. El fragmento no tiene blending: el pipeline
// no lo activa porque no hay nada que mezclar *con* la superficie, solo hay que
// resolver la transparencia de la fuente contra un color de fondo. Ver `fs_main`,
// que es donde estuvo el bug.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

// ------------------------------------------------------------------
// DÓNDE ESTÁ EL LIENZO DENTRO DE LA VENTANA.
//
// MEDIDO, y este es el bug de raíz de «no se ve nada de lo que dibujo»:
//
//   `desktop/src/app.rs:278` — `UpdateViewportPhysicalBounds`:
//
//       let viewport_offset_x = x / window_size.width as f64;
//       render_state.set_viewport_offset([viewport_offset_x, viewport_offset_y]);
//       let viewport_scale_x = if width != 0. { window_size.width as f64 / width } else { 1. };
//       render_state.set_viewport_scale([viewport_scale_x, viewport_scale_y]);
//
// El escritorio coloca la textura del motor DENTRO del rectángulo del lienzo.
// Este blit la estiraba a toda la ventana con `uv` de 0 a 1, así que el
// lienzo acababa en el sitio equivocado y lo que se dibujaba caía fuera de la
// vista. Por eso se creaba la capa y no se veía nada: el nodo existía, se
// evaluaba (`lienzo=146582`) y se pintaba en un sitio donde no se miraba.
//
// El matemático es el del compositor de upstream, `composite_shader.wgsl`:
//
//     coorde = (uv - offset) * scale
//
// con `offset` en fracción de ventana (dónde empieza) y `scale` en su inversa
// (qué fracción ocupa). Fuera de 0..1 no hay lienzo, y se pinta el fondo.
//
// El uniforme, y no `var<immediate>`: el escritorio usa la extensión de naga
// `immediates`, que aquí no está habilitada. Con un uniform buffer es lo mismo
// y no depende de la extensión.
// ------------------------------------------------------------------
struct Colocacion {
    offset: vec2f,
    scale: vec2f,
    // Modo de visualizacion. 0 = normal. Lo escribe `native_debug`, que se
    // llama desde JS con `window.GraphiteNative.debug(n)`, asi que se cambia EN
    // CALIENTE desde el navegador sin recompilar nada.
    //
    // MEDIDO, y esto contesta lo que no se puede ver en un log. La pregunta que
    // de verdad importa es "deberia haber un rectangulo aqui y no lo hay": eso no
    // se responde con contadores, se responde mirando. Los modos son:
    //
    //   1  UV:  rojo = u, verde = v. Si sale un degradado, estamos
    //           MUESTREANDO donde toca y el problema es el contenido.
    //           Si sale plano, no estamos muestreando la textura.
    //   2  ALFA de la textura. Blanco = hay algo opaco. Negro = esta vacia.
    //           ESTE es el que responde a "se crea el objeto pero no se pinta".
    //   3  RGB de la textura tal cual.
    //   4  La coordenada de la textura que estamos muestreando, en gris: dice si
    //           nos salimos del rango por el `offset`/`scale`.
    //   5  Uno por texel: cada pixel de la pantalla tiene un color distinto,
    //           con un patron fijo. Si sale liso, se estan repitiendo texels.
    //
    // El fondo fuera del lienzo se deja como el fondo del motor, para que se
    // distinga "fuera del lienzo" de "dentro del lienzo pero vacio".
    modo: f32,
    // OJO, y esto es un error que ya se cometio: el relleno son `f32` sueltos y
    // NO un `vec3f`.
    //
    // MEDIDO, al abrir la app con este shader:
    //
    //     wgpu error: Validation Error
    //       In bind group index 0, the buffer bound at binding index 2 is bound
    //       with size 32 where the shader expects 48.
    //
    // Un `vec3f` en un uniform va alineado a 16, asi que con `modo: f32` en el
    // offset 16 el `vec3f` no empieza en el 20: empieza en el 32, y la structura
    // mide 48. Cuatro `f32` sueltos si dan los 32 bytes justos.
    //
    // Y el fallo es un `abort`, porque los errores de wgpu son fatales por
    // defecto. Lo que se ve en el movil es "la app se cierra al abrir".
    _r0: f32,
    _r1: f32,
    _r2: f32,
}

@group(0) @binding(2) var<uniform> colocacion: Colocacion;

struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // 0,1,2 y 0,2,3: dos triangulos que juntos cubren el cuadrado entero.
    // Cualquier cobertura menor que esto deja partes de la superficie sin
    // escribir, que es exactamente lo que pasaba antes (ver `Engine::frame`).
    var p = array<vec2f, 6>(
        vec2f(-1.0, -1.0),
        vec2f( 1.0, -1.0),
        vec2f( 1.0,  1.0),
        vec2f(-1.0, -1.0),
        vec2f( 1.0,  1.0),
        vec2f(-1.0,  1.0),
    );
    let xy = p[i];
    var out: VsOut;
    out.pos = vec4f(xy, 0.0, 1.0);
    // V invertido: la textura tiene origen arriba-izquierda, el clip-space
    // tiene origen abajo-izquierda.
    out.uv = vec2f((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4f {
    // El fondo del motor, medido en el móvil: rgb(39,39,39).
    let fondo = vec4f(0.153, 0.153, 0.153, 1.0);

    // Fuera del rectángulo del lienzo no hay nada que mostrar. Se pinta el
    // fondo, que es lo que el escritorio hace con su `background_color`
    // (`composite_shader.wgsl`, misma comprobación antes de muestrear).
    //
    // Sin esto no se notaba, porque antes el blit estiraba la textura a toda la
    // pantalla y "tapaba" la zona de arriba. Ahora que la textura se coloca
    // donde toca, el margen es visible —y sería negro, que es justo el triángulo
    // negro que este shader arregló una vez. No se puede volver a traer.
    let coorde = (in.uv - colocacion.offset) * colocacion.scale;
    if (coorde.x < 0.0 || coorde.x > 1.0 || coorde.y < 0.0 || coorde.y > 1.0) {
        return fondo;
    }

    // Modo de visualizacion: lo que se ve aqui es DIAGNOSTICO, no la interfaz.
    let modo = i32(colocacion.modo + 0.5);
    if (modo == 1) {
        return vec4f(coorde.x, coorde.y, 0.0, 1.0);
    }
    if (modo == 2) {
        let a = textureSample(src, samp, coorde).a;
        return vec4f(a, a, a, 1.0);
    }
    if (modo == 3) {
        return textureSample(src, samp, coorde);
    }
    if (modo == 4) {
        return vec4f(coorde, 0.0, 1.0);
    }
    if (modo == 5) {
        // 16 (coordenada de texel) troceada en dos bytes por canal.
        let t = coorde * vec2f(textureDimensions(src));
        return vec4f(fract(t.x / 256.0), fract(t.y / 256.0), fract(t.x / 16.0), 1.0);
    }

    let c = textureSample(src, samp, coorde);
    // Vello entrega alfa PREMULTIPLICADA: `c.rgb` ya viene multiplicado por
    // `c.a`. En una zona transparente, `c.rgb` es 0 y `c.a` es 0.
    //
    // Por eso aquí NO se puede hacer `vec4f(c.rgb, 1.0)`: eso convierte
    // "transparente" en "negro opaco", y el resultado es un triángulo negro
    // justo donde el motor no dibujó nada. Ese fue el bug del puerto anterior,
    // y su "arreglo" (forzar alfa 1) lo conservaba en vez de quitarlo.
    //
    // Lo correcto para una superficie opaca es componer la fuente
    // premultiplicada SOBRE un fondo opaco, que es el operador `over` estándar:
    //
    //     resultado = src + dst * (1 - src.a)
    //
    // Con `dst` opaco (a = 1), la transparencia del motor revela el fondo del
    // lienzo en vez de volverse negro.
    let rgb = c.rgb + fondo.rgb * (1.0 - c.a);
    return vec4f(rgb, 1.0);
}