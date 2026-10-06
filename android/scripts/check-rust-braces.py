#!/usr/bin/env python3
"""Comprueba que los ficheros Rust tienen las llaves balanceadas.

Por qué existe:

    MEDIDO. Un `{` de menos en `android/bridge/src/lib.rs`:

        error: unexpected closing delimiter: `}`
          --> android/bridge/src/lib.rs:1339:1
        error: could not compile `graphite-android-bridge` (lib)

    Tardó **ocho minutos** en aparecer, después de compilar medio grafo de nodos.
    Es un error que se ve en un segundo mirando el fichero.

No sustituye a `rustc`, que además valida tipos. Es una puerta de entrada
barata: si esto falla, el resto del pipeline no tiene sentido, y se dice en dos
segundos en vez de en ocho minutos.

Qué mira y qué no:

- Balancea `{` `}` y descarta lo que está en `//`, en `/* */`, en cadenas `"`,
  en cadenas crudas `r#"..."#` y en literales de carácter `'{'`.
- Una cadena sin cerrar **no** se considera balanceada: es un error de sintaxis
  de todas formas, y fingir que no hay problema daría un "bien" falso.

Uso:  python3 check-rust-braces.py <fichero.rs>...
"""

import sys

ERRORES = 0


def revisar(ruta: str) -> int:
    with open(ruta, encoding="utf-8") as f:
        src = f.read()

    nivel = 0
    linea = 1
    i = 0
    n = len(src)
    # Estado del lexer. Se mantiene entre caracteres porque son todos
    # delimitadores que se pueden solapar: `//` empieza un comentario de linea,
    # `/*` uno de bloque, `r#"` una cadena cruda, `"` una normal, `'` un char.
    # `escapar` es para el backslash de las cadenas normales.
    modo = "codigo"  # codigo | linea | bloque | cadena | cruda | char
    escapes = 0     # numero de `#` de la cadena cruda en curso
    escapar = False

    while i < n:
        c = src[i]
        if c == "\n":
            linea += 1
            if modo == "linea":
                modo = "codigo"
            i += 1
            continue

        siguiente = src[i + 1] if i + 1 < n else ""

        if modo == "codigo":
            if c == "/" and siguiente == "/":
                modo = "linea"
                i += 2
                continue
            if c == "/" and siguiente == "*":
                modo = "bloque"
                i += 2
                continue
            # Cadena cruda: r"..." o r#"..."#. Un `#` de mas o de menos y el
            # lexer se desincroniza y da un balance equivocado.
            if c == "r" and (siguiente == '"' or siguiente == "#"):
                escapes = 0
                j = i + 1
                while j < n and src[j] == "#":
                    escapes += 1
                    j += 1
                if j < n and src[j] == '"':
                    modo = "cruda"
                    i = j + 1
                    continue
            if c == "b" and siguiente == '"':
                modo = "cadena"
                escapar = False
                i += 2
                continue
            if c == '"':
                modo = "cadena"
                escapar = False
                i += 1
                continue
            # Lifetime: 'a o '_. Es un `'` seguido de letra o de guion bajo, y
            # NO un char.
            #
            # MEDIDO que hizo falta este caso: `Formatter<'_>` —el lifetime
            # anonimo—. Antes el codigo solo aceptaba `'` + letra, porque en
            # Python `"_".isalpha()` es False. Asi que `'_` abria un char literal
            # fantasma, el lexer se quedaba dentro hasta el siguiente `'` del
            # fichero, y de ahi salia un "falta un `}`" que no existia:
            # `rustc` compilaba el fichero sin problema y el check lo marcaba
            # roto.
            #
            # La diferencia entre el lifetime `'_` y el char `'_'` es lo que
            # viene despues: en el char hay otra comilla, en el lifetime no.
            if c == "'" and (siguiente.isalpha() or siguiente == "_"):
                if siguiente == "_" and src[i + 2 : i + 3] == "'":
                    modo = "char"
                    i += 1
                    continue
                i += 2
                continue
            if c == "'":
                modo = "char"
                i += 1
                continue
            if c == "{":
                nivel += 1
            elif c == "}":
                nivel -= 1
                if nivel < 0:
                    print("::error::%s:%d: `}` de mas: no hay ningun `{` abierto" % (ruta, linea))
                    return 1
            i += 1
            continue

        if modo == "linea":
            i += 1
            continue

        if modo == "bloque":
            if c == "*" and siguiente == "/":
                modo = "codigo"
                i += 2
            else:
                i += 1
            continue

        if modo == "cadena":
            if escapar:
                escapar = False
            elif c == "\\":
                escapar = True
            elif c == '"':
                modo = "codigo"
            i += 1
            continue

        if modo == "cruda":
            if c == '"':
                # Cierra si vienen los `#` exactos.
                if src[i + 1 : i + 1 + escapes] == "#" * escapes:
                    modo = "codigo"
                    i += 1 + escapes
                    continue
            i += 1
            continue

        if modo == "char":
            if c == "\\":
                i += 2
                continue
            if c == "'":
                modo = "codigo"
            i += 1
            continue

    if nivel != 0:
        print("::error::%s: queda(n) %d `{` sin cerrar (falta un `}`)" % (ruta, nivel))
        return 1
    if modo in ("cadena", "cruda", "char", "bloque"):
        print("::error::%s: el fichero termina dentro de un %s sin cerrar" % (ruta, modo))
        return 1
    return 0


def main() -> int:
    # `ERRORES` es global porque se incrementa dentro del bucle de `main`. Sin
    # este `global` Python la trata como local y revienta al primer fallo —que es
    # justo cuando se necesita el mensaje. MEDIDO: `UnboundLocalError`.
    global ERRORES

    rutas = sys.argv[1:]
    if not rutas:
        print("uso: check-rust-braces.py <fichero.rs>...", file=sys.stderr)
        return 2
    for ruta in rutas:
        if revisar(ruta) == 0:
            print("  OK  %s" % ruta)
        else:
            ERRORES += 1
    if ERRORES:
        print("::error::%d fichero(s) con llaves o cadenas mal cerradas" % ERRORES)
        return 1
    print("VEREDICTO: llaves balanceadas")
    return 0


if __name__ == "__main__":
    sys.exit(main())