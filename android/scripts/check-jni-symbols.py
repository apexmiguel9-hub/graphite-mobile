#!/usr/bin/env python3
"""Comprueba que cada `external fun` de Kotlin tiene su símbolo JNI en el `.so`.

Por qué existe:

    MEDIDO en el móvil, con el frontend ya cargando y llamando al puente:

        java.lang.UnsatisfiedLinkError: No implementation found for
          dev.graphite.mobile.WebUIKt.nativeMessage(java.lang.String)
        (tried Java_dev_graphite_mobile_WebUIKt_nativeMessage)

El símbolo JNI se compone de PAQUETE + FICHERO + FUNCIÓN:

    Java_dev_graphite_mobile_<FICHERO>Kt_<función>

`nativeMessage` estaba declarada en `WebUI.kt`, así que Kotlin buscaba
`WebUIKt` y Rust exportaba `MainActivityKt`.

Lo que hace esto de differently —y la razón de existir— es que el check ANTERIOR
miraba el nombre de las funciones pero daba por hecho el `MainActivityKt` de ahí.
Las tres primeras estaban en `MainActivity.kt` y coincidían; las dos nuevas no,
y el CI daba verde.

Un `external fun` en Kotlin no da error de compilación si el símbolo no está en el
`.so`. Compila siempre. El fallo es un `UnsatisfiedLinkError` en el móvil, al
arrancar, que solo nombra el símbolo que falta.

Uso:  python3 check-jni-symbols.py <ruta/libgraphite_android.so> <dir-kotlin>
"""

import os
import re
import sys

PAQUETE = "dev.graphite_mobile"  # `dev.graphite.mobile` con puntos y comas
PREFIJO = "Java_" + PAQUETE.replace(".", "_") + "_"

# `external fun nativeX(...)`. Se buscan en todos los .kt del directorio.
RE_EXTERNAL = re.compile(r"^\s*external\s+fun\s+([A-Za-z_][A-Za-z0-9_]*)", re.M)


def kotlin_ficheros(directorio):
    for raiz, _dirs, files in os.walk(directorio):
        for f in files:
            if f.endswith(".kt"):
                yield os.path.join(raiz, f)


def main() -> int:
    if len(sys.argv) != 3:
        print("uso: check-jni-symbols.py <libgraphite_android.so> <dir-kotlin>",
              file=sys.stderr)
        return 2
    so, kotlin_dir = sys.argv[1], sys.argv[2]

    if not os.path.isfile(so):
        print("::error::no existe el .so %s" % so)
        return 1

    with open(so, "rb") as f:
        binario = f.read()

    # Del .so a texto, una sola vez. Los símbolos son ASCII y estan enteros.
    texto = binario.decode("latin-1")

    faltan = []
    total = 0
    for ruta in kotlin_ficheros(kotlin_dir):
        with open(ruta, encoding="utf-8") as f:
            fuente = f.read()
        nombre = os.path.splitext(os.path.basename(ruta))[0]
        # El prefijo lleva el NOMBRE DEL FICHERO, no la clase. `MainActivity.kt`
        # -> `MainActivityKt`, porque el fichero se compila a una clase facade con
        # ese sufijo.
        prefijo = PREFIJO + nombre + "Kt_"
        for fn in RE_EXTERNAL.findall(fuente):
            total += 1
            simbolo = prefijo + fn
            if simbolo in texto:
                print("  OK  %s  (%s)" % (fn, ruta))
            else:
                faltan.append((fn, ruta, simbolo))

    if faltan:
        for fn, ruta, simbolo in faltan:
            print("::error::%s (declarada en %s) no tiene el símbolo %s"
                  % (fn, ruta, simbolo))
            print("::error::  falta en Rust:")
            print('::error::    #[export_name = "%s"]' % simbolo)
        print("::error::%d de %d funciones nativas sin implementación"
              % (len(faltan), total))
        return 1

    if total == 0:
        print("::error::no se encontró ninguna `external fun`: el grep está roto")
        return 1

    print("VEREDICTO: las %d funciones nativas tienen su símbolo JNI" % total)
    return 0


if __name__ == "__main__":
    sys.exit(main())