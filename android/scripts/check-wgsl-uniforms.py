#!/usr/bin/env python3
"""Comprueba que los uniformes WGSL coinciden con el tamaño del buffer de Rust.

Por qué existe:

    MEDIDO. Al añadir un `f32` de modo de diagnóstico al uniform `Colocacion` de
    `android/bridge/src/blit.wgsl`, se rellenó con un `vec3f` "para cuadrar":

        struct Colocacion {
            offset: vec2f,     // offset 0
            scale: vec2f,      // offset 8
            modo: f32,         // offset 16
            _relleno: vec3f,   // <-- alineado a 16, empieza en el 32
        }

    Un `vec3f` en un uniform va alineado a 16. El buffer se reservó con 32 bytes:

        wgpu error: Validation Error
          In bind group index 0, the buffer bound at binding index 2 is bound with
          size 32 where the shader expects 48.

    Los errores de wgpu son FATALES por defecto, así que en el móvil lo que se ve
    es que la app se cierra al abrir, sin más. Y elSizes-dismatch no lo caza
    `cargo check` — compila perfectamente. Ni el compilador de WGSL: el shader es
    válido; lo que no cuadra es el tamaño del buffer, que vive en Rust.

No sustituye a `wgpu`, que valida de verdad. Es una puerta barata: si esto falla,
el uniform está mal y no tiene sentido seguir compilando.

Qué calcula: el desplazamiento de cada miembro según las reglas de alineación de
WGSL (un `vec2` alinea a 8, un `vec3`/`vec4` a 16, un escalar al tamaño suyo, y
el hueco se rellena hasta el siguiente múltiplo) y compara el tamaño total con el
que reserva Rust.

Uso:  python3 check-wgsl-uniforms.py <shader.wgsl> <struct> <bytes-reservados>
"""

import re
import sys

# alineacion por tipo, en bytes
ALINEACION = {"f32": 4, "i32": 4, "u32": 4, "vec2f": 8, "vec3f": 16, "vec4f": 16}


def taille(tipo: str) -> int:
    return {"f32": 4, "i32": 4, "u32": 4, "vec2f": 8, "vec3f": 12, "vec4f": 16}[tipo]


def struct_offsets(cuerpo: str):
    """(nombre, tipo, offset) de cada miembro, en orden."""
    limpio = re.sub(r"//.*", "", cuerpo)
    limpio = re.sub(r"/\*.*?\*/", "", limpio, flags=re.S)
    miembros = []
    off = 0
    for nombre, tipo in re.findall(r"([A-Za-z_][A-Za-z0-9_]*)\s*:\s*([A-Za-z0-9_]+)\s*,", limpio):
        if tipo not in ALINEACION:
            raise SystemExit("::error::tipo `%s` en el uniform; el check no lo conoce" % tipo)
        a = ALINEACION[tipo]
        off = (off + a - 1) // a * a          # subir al siguiente multiplo de la alineacion
        miembros.append((nombre, tipo, off))
        off += taille(tipo)
    return miembros


def main() -> int:
    if len(sys.argv) != 4:
        print("uso: check-wgsl-uniforms.py <shader.wgsl> <Struct> <bytes>", file=sys.stderr)
        return 2

    shader, nombre, reservado = sys.argv[1], sys.argv[2], int(sys.argv[3])

    with open(shader, encoding="utf-8") as f:
        src = f.read()
    m = re.search(r"struct\s+%s\s*\{(.*?)\}" % re.escape(nombre), src, re.S)
    if not m:
        print("::error::%s: no se encuentra `struct %s`" % (shader, nombre))
        return 1

    miembros = struct_offsets(m.group(1))
    if not miembros:
        print("::error::%s: `%s` no tiene miembros" % (shader, nombre))
        return 1

    fin = max(o + taille(t) for _, t, o in miembros)
    align = 16
    total = (fin + align - 1) // align * align

    for nombre_m, tipo, off in miembros:
        print("  %-10s %-6s offset %3d" % (nombre_m, tipo, off))
    print("  %-10s %-6s %5d bytes (alineado a %d)" % ("TOTAL", "", total, align))
    print("  Rust reserva %d bytes" % reservado)

    if total != reservado:
        print("::error::el shader espera %d bytes y Rust reserva %d" % (total, reservado))
        print("::error::  en wgpu eso es: 'the buffer ... is bound with size %d where "
              "the shader expects %d'" % (reservado, total))
        print("::error::  y es un ABORT, porque los errores de wgpu son fatales.")
        if total > reservado:
            print("::error::  suele ser un `vec3f`/`vec4f` de relleno: alinea a 16 y "
                  "empieza en el siguiente multiplo de 16, no justo despues del `f32`.")
            print("::error::  relleno con `f32` sueltos.")
        return 1

    print("VEREDICTO: %s son %d bytes y Rust reserva %d" % (nombre, total, reservado))
    return 0


if __name__ == "__main__":
    sys.exit(main())