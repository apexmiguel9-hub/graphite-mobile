#!/usr/bin/env python3
"""Cliente de Chrome DevTools para inspeccionar el WebView por adb.

Por qué existe:

    MEDIDO. Con el frontend arrancado, los mensajes fluyendo (el motor contesta
    con 328 KB) y la pantalla en negro, no había forma de saber si el WebView
    estaba pintando el DOM y el DOM estaba vacio, o si el WebView no pintaba.
    Adb logcat solo da la consola, y ahi no hay ningun error: el arranque es
    limpio y silencioso.

    El puerto de DevTools del WebView SI existe:

        /proc/net/unix -> webview_devtools_remote_<pid>

    y por ahi se puede ejecutar JavaScript en la pagina REAL del movil. Es la
    unica forma de mirar lo que hay dentro sin recompilar nada, y por eso
    compensa el trabajo de escribir un cliente WebSocket a mano.

Uso:

    # Engancha el puerto (hace falta el PID del proceso).
    adb forward tcp:9222 localabstract:webview_devtools_remote_$(adb shell pidof dev.graphite.mobile)

    python3 cdp.py 'document.body.innerHTML.length'
    python3 cdp.py --dom          # el arbol del cuerpo, recortado
    python3 cdp.py --console      # activa la consola y la imprime
    python3 cdp.py --shot f.png   # captura por DevTools
"""

import base64
import json
import os
import socket
import struct
import sys
import time
import urllib.request

DEVTOOLS = "http://127.0.0.1:9222"


# ---------------------------------------------------------------- WebSocket
class WS:
    """Lo justo de RFC 6455 para hablar con DevTools: texto, sin fragmentar.

    No hay mascar para lo que llega (el servidor no mascara) y sí para lo que
    mandamos (obligatorio). Sin SDL, sin extensiones, sin ping.
    """

    def __init__(self, url: str, timeout: float = 20.0):
        assert url.startswith("ws://"), url
        resto = url[len("ws://") :]
        hostport, _, path = resto.partition("/")
        host, _, port = hostport.partition(":")
        self.sock = socket.create_connection((host, int(port or 80)), timeout=timeout)

        clave = base64.b64encode(os.urandom(16)).decode()
        peticion = (
            "GET /%s HTTP/1.1\r\n"
            "Host: %s\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            "Sec-WebSocket-Key: %s\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n" % (path, hostport, clave)
        )
        self.sock.sendall(peticion.encode())

        # Cabeceras de la respuesta, hasta la linea vacia.
        cab = b""
        while b"\r\n\r\n" not in cab:
            trozo = self.sock.recv(4096)
            if not trozo:
                raise RuntimeError("el servidor cerro antes de completar el handshake")
            cab += trozo
        if b"101" not in cab.split(b"\r\n")[0]:
            raise RuntimeError("no hubo upgrade a websocket: %s" % cab.split(b"\r\n")[0])

    def _leer(self, n: int) -> bytes:
        out = b""
        while len(out) < n:
            trozo = self.sock.recv(n - len(out))
            if not trozo:
                raise RuntimeError("cerrado a mitad de un frame")
            out += trozo
        return out

    def recv(self) -> str:
        while True:
            cab = self._leer(2)
            op = cab[0] & 0x0F
            largo = cab[1] & 0x7F
            if largo == 126:
                largo = struct.unpack(">H", self._leer(2))[0]
            elif largo == 127:
                largo = struct.unpack(">Q", self._leer(8))[0]
            datos = self._leer(largo)
            if op == 0x1:
                return datos.decode("utf-8", "replace")
            if op == 0x8:
                raise RuntimeError("el servidor cerro el websocket")
            # 0x9 ping, 0xA pong, 0x0 continuacion: aqui no ocurren.

    def send(self, texto: str) -> None:
        payload = texto.encode()
        mascara = os.urandom(4)
        n = len(payload)
        cab = bytearray([0x81])
        if n < 126:
            cab.append(0x80 | n)
        elif n < 65536:
            cab.append(0x80 | 126)
            cab += struct.pack(">H", n)
        else:
            cab.append(0x80 | 127)
            cab += struct.pack(">Q", n)
        cab += mascara
        cab += bytes(b ^ mascara[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(bytes(cab))

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


# ---------------------------------------------------------------- CDP
class CDP:
    def __init__(self, timeout: float = 20.0):
        with urllib.request.urlopen(DEVTOOLS + "/json/list", timeout=timeout) as r:
            paginas = json.load(r)
        paginas = [p for p in paginas if p.get("type") == "page" and p.get("webSocketDebuggerUrl")]
        if not paginas:
            raise SystemExit(
                "no hay ninguna pagina en DevTools. Esta el WebView arrancado?\n"
                "  adb forward tcp:9222 localabstract:webview_devtools_remote_$(adb shell pidof dev.graphite.mobile)"
            )
        # Si hay varias, la primera. En este proyecto solo hay un WebView.
        self.url = paginas[0]["webSocketDebuggerUrl"]
        self.ws = WS(self.url, timeout)
        self.id = 0
        self.url_pagina = paginas[0]["url"]

    def call(self, metodo: str, params: dict | None = None, timeout: float = 20.0):
        """Envia un comando y espera SU respuesta, no la primera que llegue.

        DevTools multiplexa eventos (`Runtime.consoleAPICalled`,
        `Page.loadEventFired`) con las respuestas. Sin mirar el `id` se
        devuelve el evento equivocado, y es un fallo silencioso.
        """
        self.id += 1
        mio = self.id
        self.ws.send(json.dumps({"id": mio, "method": metodo, "params": params or {}}))
        fin = time.time() + timeout
        while time.time() < fin:
            msg = json.loads(self.ws.recv())
            if msg.get("id") == mio:
                if "error" in msg:
                    raise RuntimeError("%s: %s" % (metodo, msg["error"]))
                return msg.get("result", {})
        raise TimeoutError(metodo)

    def eval(self, expresion: str, timeout: float = 20.0):
        """Ejecuta JS y devuelve el valor,(values by value, sin referencias)."""
        r = self.call(
            "Runtime.evaluate",
            {
                "expression": expresion,
                "returnByValue": True,
                "awaitPromise": True,
                "userGesture": True,
            },
            timeout,
        )
        if r.get("exceptionDetails"):
            raise RuntimeError(json.dumps(r["exceptionDetails"], indent=2)[:1200])
        return r.get("result", {}).get("value")

    def close(self):
        self.ws.close()


# ---------------------------------------------------------------- comandos
EXPRESIONES = {
    # Lo que hay dentro de la pagina, de un vistazo.
    "dom": "document.body.innerHTML",
    "hijos": "document.body.children.length",
    "texto": "document.body.innerText.slice(0,400)",
    # Si el frontend ha montado: MainWindow existe, y con el cuantos hijos.
    "montado": "!!document.querySelector('.main-window') || document.body.innerHTML.length",
    # Errores de la consola acumulados desde que arranco la pagina.
    "errores": "window.__errores || '(no instrumentado)'",
    # Lo que hay pegado al body: mide si hay contenido o solo el fondo.
    "cuerpo": """(() => {
        const b = document.body;
        return JSON.stringify({
            hijos: b.children.length,
            html: b.innerHTML.slice(0, 300),
            rect: b.getBoundingClientRect().toJSON(),
            primerHijo: b.firstElementChild ? {
                tag: b.firstElementChild.tagName,
                clase: b.firstElementChild.className,
                rect: b.firstElementChild.getBoundingClientRect().toJSON()
            } : null
        }, null, 1);
    })()""",
}


def main() -> int:
    args = sys.argv[1:]
    c = CDP()
    try:
        if args and args[0] == "--dom":
            print(c.eval(EXPRESIONES["dom"]))
        elif args and args[0] == "--cuerpo":
            print(c.eval(EXPRESIONES["cuerpo"]))
        elif args and args[0] == "--shot":
            destino = args[1] if len(args) > 1 else "/tmp/cdp.png"
            r = c.call("Page.captureScreenshot", {"format": "png"})
            with open(destino, "wb") as f:
                f.write(base64.b64decode(r["data"]))
            print("captura en", destino)
        elif args and args[0] == "--console":
            # Instrumenta y luego lee: asi tambien pilla lo que ya paso.
            c.eval("""
                window.__errores = window.__errores || [];
                if (!window.__cdp_instrumentado) {
                    window.__cdp_instrumentado = true;
                    const oe = console.error.bind(console);
                    console.error = (...a) => { window.__errores.push(a.join(' ')); oe(...a); };
                    window.addEventListener('error', e => window.__errores.push('ERROR ' + e.message));
                    window.addEventListener('unhandledrejection',
                        e => window.__errores.push('RECHAZO ' + (e.reason && e.reason.message || e.reason)));
                }
                'ok'
            """)
            import json as _json
            print(_json.dumps(c.eval(EXPRESIONES["errores"]), indent=1, ensure_ascii=False))
        else:
            expresion = args[0] if args else EXPRESIONES["cuerpo"]
            v = c.eval(expresion)
            print(v if isinstance(v, str) else __import__("json").dumps(v, indent=1, ensure_ascii=False))
    finally:
        c.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
