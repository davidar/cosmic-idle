// Layer shell surface that fades to black, before setting DPMS off.

use keyframe::{ease, functions::EaseInOut};
use std::{
    fs::OpenOptions,
    io::Write,
    os::fd::AsFd,
    path::PathBuf,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    protocol::{wl_buffer, wl_callback, wl_output, wl_pointer, wl_shm, wl_shm_pool, wl_surface},
};
use wayland_protocols::wp::{
    single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1,
    viewporter::client::wp_viewport,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::{State, StateInner};

const FADE_TIME: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct FadeBlackSurface {
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    viewport: wp_viewport::WpViewport,
    has_first_configure: bool,
    started: Instant,
}

impl FadeBlackSurface {
    pub fn new(inner: &StateInner, output: &wl_output::WlOutput) -> Self {
        let surface = inner.compositor.create_surface(&inner.qh, ());
        let layer_surface = inner.layer_shell.get_layer_surface(
            &surface,
            Some(output),
            zwlr_layer_shell_v1::Layer::Overlay,
            "fade-to-black".to_string(),
            &inner.qh,
            (),
        );
        layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::all());
        layer_surface.set_exclusive_zone(-1);
        let viewport = inner.viewporter.get_viewport(&surface, &inner.qh, ());
        surface.commit();
        Self {
            surface,
            layer_surface,
            viewport,
            has_first_configure: false,
            started: Instant::now(),
        }
    }

    pub fn is_done(&self) -> bool {
        self.started.elapsed() > FADE_TIME
    }

    fn configure(&mut self, inner: &StateInner, width: u32, height: u32) {
        self.viewport.set_destination(width as i32, height as i32);
        if !self.has_first_configure {
            self.update(inner);
            self.has_first_configure = true;
        }
    }

    pub fn update(&self, inner: &StateInner) {
        let time = self.started.elapsed().as_secs_f64() / FADE_TIME.as_secs_f64();
        let alpha = ease(EaseInOut, 0., u32::MAX as f64, time) as u32;
        let buffer = match &inner.single_pixel_buffer_manager {
            Some(manager) => manager.create_u32_rgba_buffer(0, 0, 0, alpha, &inner.qh, ()),
            None => match shm_black_pixel(inner, alpha) {
                Ok(buffer) => buffer,
                Err(err) => {
                    log::error!("failed to create fade-to-black buffer: {}", err);
                    return;
                }
            },
        };
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.frame(&inner.qh, self.surface.clone());
        self.surface.damage(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
        buffer.destroy();
    }
}

// Fallback for compositors without wp-single-pixel-buffer (niri): a 1x1
// ARGB8888 wl_shm buffer holding the same premultiplied black pixel. The
// viewport scales it to the output exactly like the single-pixel buffer.
fn shm_black_pixel(inner: &StateInner, alpha: u32) -> std::io::Result<wl_buffer::WlBuffer> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("cosmic-idle-fade-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;
    // The descriptor is the only handle anyone needs; a fresh file per frame
    // keeps the compositor's mapping of the previous one untouched.
    let _ = std::fs::remove_file(&path);
    let pixel = [0u8, 0, 0, (alpha >> 24) as u8]; // little-endian ARGB: B, G, R, A
    file.write_all(&pixel)?;
    let pool = inner
        .shm
        .create_pool(file.as_fd(), pixel.len() as i32, &inner.qh, ());
    let buffer = pool.create_buffer(
        0,
        1,
        1,
        pixel.len() as i32,
        wl_shm::Format::Argb8888,
        &inner.qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

impl Drop for FadeBlackSurface {
    fn drop(&mut self) {
        self.viewport.destroy();
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        obj: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                for output in &mut state.outputs {
                    if let Some(fade_surface) = &mut output.fade_surface
                        && &fade_surface.layer_surface == obj
                    {
                        fade_surface.layer_surface.ack_configure(serial);
                        fade_surface.configure(&state.inner, width, height);
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, wl_surface::WlSurface> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        surface: &wl_surface::WlSurface,
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_callback::Event::Done { callback_data: _ } => {
                for output in &mut state.outputs {
                    if let Some(fade_surface) = &mut output.fade_surface
                        && &fade_surface.surface == surface
                    {
                        if fade_surface.is_done() {
                            // All outputs are done fading
                            if state
                                .outputs
                                .iter()
                                .flat_map(|o| o.fade_surface.as_ref())
                                .all(|s| s.is_done())
                            {
                                state.fade_done();
                            }
                        } else {
                            fade_surface.update(&state.inner);
                        }
                        break;
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        _: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface: _,
                surface_x: _,
                surface_y: _,
            } => {
                // The only surface in our client is `FadeBlackSurface`.
                // So hide the cursor if entered.
                pointer.set_cursor(serial, None, 0, 0);
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
