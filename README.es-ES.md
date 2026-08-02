

<p align="center">
  <img src="src/assets/logo.png" alt="The Moaning Guy" width="280" />
</p>

<h1 align="center">The Moaning Guy</h1>

<p align="center">
  Reproduce sonidos de gemidos cuando le das una bofetada a tu portátil. Inspirado por <a href="https://slapmac.com">SlapMac</a> - pero para Windows, macOS y Linux.
</p>

<p align="center">
  <img src="https://img.shields.io/github/sponsors/adityapatel-00" />
  <img src="https://img.shields.io/github/downloads/adityapatel-00/themoaningguy/total" />
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Tauri-v2-7c4dff?style=flat-square" alt="Tauri v2" />
  <img src="https://img.shields.io/badge/Rust-Backend-orange?style=flat-square" alt="Rust" />
  <img src="https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-blue?style=flat-square" alt="Cross-platform" />
</p>

---

## Cómo Funciona

Cuando esté disponible, **The Moaning Guy puede utilizar un acelerómetro / sensor de movimiento integrado** para una detección de golpes más precisa. En dispositivos sin soporte de sensor, vuelve automáticamente al **detector de micrófono**.

Un golpe en el chasis del portátil produce un impulso agudo y corto que es fácil de distinguir del audio normal. La aplicación escucha en tiempo real, detecta picos de movimiento o picos de amplitud del micrófono por encima de un umbral configurable y reproduce un sonido aleatorio de tu paquete seleccionado.

```text
Mic Input -> Amplitude Analysis -> Spike Detection -> Sound Playback
             (cpal)              (threshold + cooldown)    (rodio)
```

- **El volumen se escala con la fuerza** - golpe más fuerte = gemido más alto
- **Bolsa de mezcla** - reproduce todos los sonidos antes de repetir alguno
- **Sin superposición** - un nuevo disparador detiene el sonido anterior
- **Modo acelerómetro** - utiliza hardware de sensor compatible para una detección de golpes más precisa
- **Respaldo de micrófono** - funciona en dispositivos sin sensor de movimiento
- **Reglas de puerto** - activa paquetes al cargar, almacenamientos USB, pantallas externas, Ethernet y eventos de dock
- **Sensibilidad ajustable** - configúrala para tu entorno
- **Temporizador de enfriamiento (cooldown)** - evita disparos rápidos consecutivos

## Arquitectura

```text
+-----------------------------------------+
|              System Tray                |
|   Pause/Resume · Test · Settings · Quit |
+--------------+--------------------------+
               |
       +-------v--------+    <- HTML/CSS/JS (Tauri webview)
       |  Settings UI   |
       | (settings.html)|
       +-------+--------+
               | IPC (invoke/emit)
       +-------v--------+
       |   Rust Backend |
       |                |
       |  + Detector +  |    <- Dedicated thread, cpal mic input
       |  | Threshold | |
       |  | Cooldown  | |
       |  | on_slap() | |
       |  +-----+-----+ |
       |        |       |
       |  +-----v-----+ |    <- Dedicated thread, rodio output
       |  |  Player   | |
       |  | Shuffle   | |
       |  | Single    | |
       |  +-----------+ |
       |                |
       |  + Settings +  |    <- Arc<Mutex<Settings>>
       |  | JSON disk | |
       |  +-----------+ |
       +--------+-------+
                |
       +--------v--------+    <- %APPDATA% / ~/Library / ~/.local/share
       |   App Data Dir  |
       |   sounds/       |
       |     bundle-a/   |
       |     bundle-b/   |
       |   settings.json |
       +-----------------+
```

## Stack Tecnológico

| Capa | Tecnología |
|-------|-----------|
| Marco de trabajo | [Tauri v2](https://tauri.app) |
| Backend | Rust |
| Frontend | HTML / CSS / JS puro (Vanilla) |
| Entrada de Audio | [cpal](https://crates.io/crates/cpal) |
| Salida de Audio | [rodio](https://crates.io/crates/rodio) |
| Diálogo de Archivos | [tauri-plugin-dialog](https://crates.io/crates/tauri-plugin-dialog) |

## Configuración

### Requisitos previos

- [Node.js](https://nodejs.org) (v18+)
- [Rust](https://rustup.rs) (última versión estable)
- [Requisitos previos de Tauri v2](https://v2.tauri.app/start/prerequisites/)

**Solo Linux:**
```bash
sudo apt install libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libasound2-dev
```

### Desarrollo

```bash
git clone https://github.com/adityapatel-00/themoaningguy.git
cd themoaningguy
npm install
npm run dev
```

### Construir

```bash
npm run build
```

Genera instaladores específicos de la plataforma en `src-tauri/target/release/bundle/`.

## Uso

1. Inicia la aplicación - residirá en tu **bandeja del sistema**
2. Haz clic derecho en el icono de la bandeja -> **Ajustes**
3. Crea un **paquete de sonidos** e importa tus archivos de audio (`wav`, `mp3`, `ogg`, `flac`)
4. Elige el modo **Acelerómetro** o **Micrófono** cuando esté disponible
5. Configura las reglas de **Detección de Puertos** para eventos de conexión/desconexión
6. Ajusta la **sensibilidad**, **cooldown** y el **volumen**
7. Guarda - y luego dale una bofetada a tu portátil

La pantalla de ajustes también incluye un pequeño aviso de soporte y enlaces en el pie de página para GitHub Sponsors, Ko-fi, UPI (`x.pulseop@axl`) y dar estrella al repositorio.

## Seguridad y Limitaciones

- La detección se realiza con el mejor esfuerzo posible y puede producir falsos positivos o pasar por alto algunos eventos.
- La detección de puertos depende de lo que el sistema operativo exponga en el dispositivo actual.
- El modo micrófono utiliza el dispositivo de entrada activo mientras la aplicación se esté ejecutando.
- El modo acelerómetro, cuando está disponible, depende del hardware de sensor compatible y sus controladores.
- La monitorización en segundo plano consume algo de CPU y batería, especialmente en modo micrófono.
- Utiliza la aplicación con responsabilidad en máquinas sensibles, compartidas o de producción.

## Descargo de responsabilidad

Este software se proporciona "tal cual", sin garantía de ningún tipo. El autor no se hace responsable de ningún daño, pérdida de datos, problemas de hardware o comportamiento no deseado que pueda resultar del uso de la aplicación.

## Añadir Sonidos

La aplicación se entrega sin sonidos. Tú los proporcionas:

1. Abre Ajustes -> crea un paquete (por ejemplo, `anime` o `dramatic`)
2. Haz clic en **+ Añadir archivos de sonido** dentro del paquete
3. Selecciona archivos de audio desde tu máquina (`wav`, `mp3`, `ogg`, `flac`)
4. Selecciona el paquete como activo y Guarda

Los sonidos se almacenan en tu directorio de datos de la aplicación y persisten entre actualizaciones.

## Sitio del Proyecto

La página de aterrizaje de GitHub Pages se encuentra en [docs/index.html](docs/index.html). Habilita GitHub Pages desde la carpeta `docs/` del repositorio para publicarla.

## Notas de la Plataforma

| Plataforma | Icono de Bandeja | Notas |
|----------|-----------|-------|
| **Windows** | Funciona sin configuración | Aparece en la bandeja del sistema |
| **macOS** | Funciona sin configuración | Aparece en la barra de menú. Es posible que debas otorgar permiso de **Micrófono** en Configuración del Sistema -> Privacidad y Seguridad |
| **Linux** | Generalmente funciona | Si el icono de la bandeja no aparece en GNOME, instala la [extensión AppIndicator](https://extensions.gnome.org/extension/615/appindicator-support/) |

## Licencia

MIT
