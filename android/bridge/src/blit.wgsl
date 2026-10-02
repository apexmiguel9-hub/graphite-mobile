// Blit: lleva la textura que ha renderizado el motor a la superficie del móvil.
//
// Es un quad de pantalla completa generado con `vertex_index` (sin vertex
// buffer). Dos triangulos que cubren el clip-space entero [-1, 1].
//
// Sobre el alfa: el motor compone con alfa PREMULTIPLICADA (Vello), y la
// superficie del móvil es opaca. Por eso no hay blending en el pipeline y este
// shader devuelve alfa 1 con el color tal cual. Sin blending, el fragmento
// REEMPLAZA el destino, así que el valor de alfa no influence en nada que se
// vea.

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
    // Alfa 1: la superficie es opaca y no hay nada detrás que mostrar.
    return vec4f(c.rgb, 1.0);
}