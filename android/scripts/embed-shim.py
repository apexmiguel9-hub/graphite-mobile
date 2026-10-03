#!/usr/bin/env python3
"""Incrusta el shim JS dentro de WebUI.kt, sustituyendo un marcador.

Por que existe: el shim son 130 lineas de JavaScript. Editarlas dentro de una
cadena de Kotlin es una condicion en la que todo el mundo acaba escribiendo el
delimitador de tres comillas sin querer y rompe el fichero. Asi que el
JavaScript se versiona como JavaScript, y este script lo mete en el Kotlin al
empaquetar.

El marcador SHIM_PLACEHOLDER esta en el .kt versionado. Solo se sustituye en la
copia de trabajo de CI, que no se empuja.

Por que un script y no un heredoc dentro del bloque `run: |` de YAML:

    MEDIDO: dentro del bloque `run: |`, un heredoc queda indentado con el
    bloque, y sus lineas empiezan por 10 espacios. YAML los interpreta como un
    mapeo del paso y no como texto, asi que el workflow entero deja de ser
    valido. El sintoma es particularly confuso: GitHub no crea NINGUN job, el
    run aparece como fallido y no hay log. Nada senala al YAML.

Con un script aparte no hay nada que indentar.

Uso:  python3 embed-shim.py <ruta/WebUI.kt>
"""

import sys

SHIM = "android/app/src/main/assets/bridge.js"
TRIPLES = chr(34) * 3
MARCADOR = TRIPLES + "SHIM_PLACEHOLDER" + TRIPLES

# Las tres funciones que el wasm busca en `window`. Si el shim deja de definir
# alguna, el frontend no arranca: `sendNativeMessage` se busca con `Reflect::get`
# y un `undefined` ahi es un error de JavaScript en el movil, no un aviso.
REQUERIDAS = [
    "window.sendNativeMessage",
    "window.receiveNativeMessageData",
    "window.initializeNativeCommunication",
]


def main() -> int:
    if len(sys.argv) != 2:
        print("uso: embed-shim.py <ruta/WebUI.kt>", file=sys.stderr)
        return 2
    destino = sys.argv[1]

    try:
        with open(SHIM, encoding="utf-8") as f:
            shim = f.read()
    except OSError as e:
        print("::error::no se pudo leer %s: %s" % (SHIM, e))
        return 1

    try:
        with open(destino, encoding="utf-8") as f:
            kotlin = f.read()
    except OSError as e:
        print("::error::no se pudo leer %s: %s" % (destino, e))
        return 1

    n = kotlin.count(MARCADOR)
    if n != 1:
        print("::error::se esperaba 1 marcador en %s, hay %d" % (destino, n))
        print("::error::el .kt versionado debe tener exactamente un marcador")
        return 1

    # En un raw string de Kotlin el delimitador de tres comillas cierra la cadena.
    # El shim no lo tiene, pero escaparlo hace que esto no dependa de que siga
    # sin haberlo.
    escapado = shim.replace(TRIPLES, "\\" + TRIPLES)

    faltan = [f for f in REQUERIDAS if f not in escapado]
    if faltan:
        print("::error::el shim incrustado no define: " + ", ".join(faltan))
        print("::error::el wasm busca esas funciones en window; sin ellas no arranca")
        return 1

    if "$" in escapado:
        print("::error::el shim contiene $, que en un raw string de Kotlin pide")
        print("::error::escaparlo. El shim no deberia usarlo.")
        return 1

    salida = kotlin.replace(MARCADOR, TRIPLES + "\n" + escapado + "\n" + TRIPLES)
    try:
        with open(destino, "w", encoding="utf-8") as f:
            f.write(salida)
    except OSError as e:
        print("::error::no se pudo escribir %s: %s" % (destino, e))
        return 1

    print("  shim incrustado: %d bytes, %d globals" % (len(escapado), len(REQUERIDAS)))
    return 0


if __name__ == "__main__":
    sys.exit(main())