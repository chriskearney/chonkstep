# wm-darwin: Chonkstep's Darwin back end

This crate runs Chonkstep's window manager (`wm-core`) and theme engine (`wm-theme`) as a Wayland compositor on Darwin, for SiliconForest on Apple Silicon. It speaks real Wayland over a Unix socket through the same vendored Smithay the Linux session uses. It draws either through the boot framebuffer (SFGOP) or on the GPU with KMS page flips, and it reads keyboards and pointers from the kernel's HID character devices (SFHID).

It is experimental. Nothing here is built unless you ask for it: the whole crate sits behind the `darwin-backend` feature, and default features are empty. A Linux build of the workspace doesn't compile a line of it, and no Linux crate depends on it.

## What's here

| Module | What it does |
|---|---|
| `server` | Smithay state: xdg-shell, server-side decorations, wl_shm, linux-dmabuf v3, outputs, seat, frame callbacks |
| `scene`, `damage`, `render` | The display list, damage tracking, and the CPU renderers (`damage`, plus `legacy` full-frame for comparison) |
| `glrender`, `gpu`, `kms` | The GLES renderer on KMS page flips, EGL/libdrm loading, and a `--gl-probe` step for bringing up a new GPU stack |
| `presenter`, `output`, `pixels` | The framebuffer presenter (8-bit and 10-bit RGB, volatile stores plus a store barrier), output selection, and shm copies that never take a Rust reference to client memory |
| `hid`, `input` | SFHID keyboard and pointer records into a `wl_seat`, Control-click as a secondary click |
| `power`, `volume` | Brightness keys, idle display sleep, and volume keys through coreaudiod |
| `scale` | One display scale for the whole desk, live |
| `bench` | A fixed rendering benchmark (`chonkstep-darwin --bench`) |

The desk profile from the SiliconForest session isn't in this crate yet: the root and window menus, notices, managed launches and the settings menu. It needs `chonk-shell`'s standalone profile first, so it comes in a later pull request.

## Building and testing

On Linux, the crate's own tests run against an in-memory framebuffer and real Wayland clients over a socket pair:

```sh
cargo clippy -p wm-darwin --features darwin-backend --all-targets
cargo test -p wm-darwin --features darwin-backend
```

`CHONKSTEP_DARWIN_TEST_GL=1` runs the wire tests on the GL renderer off-screen when the machine has GLES 3.

Type-checking the Darwin side needs only the target's standard library:

```sh
rustup target add aarch64-apple-darwin
cargo clippy -p wm-darwin --features darwin-backend --all-targets --target aarch64-apple-darwin
```

Linking a binary needs a Darwin linker and SDK, plus SiliconForest's static `libSFGOP.a` and `libIOKit.a` and `libxkbcommon` for the target, passed with `-L native=...`. No Apple SDK or proprietary library is needed:

```sh
cargo build --release -p wm-darwin --features darwin-backend --bin chonkstep-darwin --target aarch64-apple-darwin
```

`IOObjectConformsTo` is the three-argument, `kern_return_t` form from SiliconForest's minimal IOKit. It is not Apple's two-argument boolean form, which is why IOKit is linked statically.

## Running

```sh
chonkstep-darwin --run --socket /private/runtime/wayland-0 [--input sfhid|sfhid-keyboard]
```

- The socket's directory must belong to the calling user, with mode 0700. Clients find the socket through `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`, and nothing is launched for you.
- `--input sfhid` opens `/dev/usb_hid_kbd` and `/dev/usb_hid_mouse` and advertises a keyboard and a pointer. `--input sfhid-keyboard` opens only the keyboard. With no `--input`, there's no `wl_seat` at all.
- `CHONKSTEP_DARWIN_RENDERER=gl|cpu|legacy|auto` picks the renderer. `auto` is the default: GL on KMS when the GPU stack is there, otherwise the CPU renderer on the framebuffer.
- `CHONKSTEP_DARWIN_GL_LIBDIR` says where to load EGL, GLES and libdrm from.
- `CHONKSTEP_DARWIN_FLIP_WAIT` and `CHONKSTEP_DARWIN_DMABUF_WAIT` tune how page flips and client dma-buf fences are waited on.
- `CHONKSTEP_DARWIN_IDLE_BLANK_MINUTES` (or `_SECONDS`) sets idle display sleep.
- Setting `CHONKSTEP_DARWIN_FRAME_STATS` prints per-frame timing.
- SIGINT and SIGTERM end the main loop. The frame in flight lands, the framebuffer is unmapped, and the connection closes.

The default theme, NeXTSTEP Classic, asks for DejaVu Sans. Put a font into `/Library/Fonts` or another path fontdb searches on Darwin.

`chonkstep-darwin --bench --help` lists the benchmark scenarios. `--offscreen` renders into memory, so it can run beside a live session.
