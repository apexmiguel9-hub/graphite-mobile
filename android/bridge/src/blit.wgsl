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
    let c = textureSample(src, samp, in.uv);
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
    let fondo = vec4f(0.02, 0.02, 0.02, 1.0);
    let rgb = c.rgb + fondo.rgb * (1.0 - c.a);
    return vec4f(rgb, 1.0);
}