use anyhow::{Context, Result};
use cairo::{Context as CairoContext, Format, ImageSurface};
use log::{error, info};
use std::{fs, os::unix::io::{AsRawFd, BorrowedFd}, time::{Duration, Instant}};
use wayland_client::{
    protocol::{wl_compositor, wl_surface, wl_shm, wl_shm_pool, wl_buffer, wl_registry},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1, zwlr_layer_surface_v1,
};
use wayland_protocols::xdg::shell::client::xdg_wm_base;

// Application state
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    shm: Option<wl_shm::WlShm>,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    width: u32,
    height: u32,
}

impl App {
    fn new() -> Self {
        App {
            compositor: None,
            layer_shell: None,
            xdg_wm_base: None,
            shm: None,
            surface: None,
            layer_surface: None,
            width: 800,
            height: 600,
        }
    }

    fn render(&self, qhandle: &QueueHandle<Self>) -> Result<()> {
        info!("Render called with dimensions: {}x{}", self.width, self.height);
        
        if self.surface.is_none() || self.shm.is_none() {
            error!("Missing surface or shm in render");
            return Ok(());
        }

        let surface = self.surface.as_ref().unwrap();
        let shm = self.shm.as_ref().unwrap();
        
        info!("Starting render process...");

        // Create a Cairo surface
        let mut cairo_surface = ImageSurface::create(Format::ARgb32, self.width as i32, self.height as i32)
            .context("Failed to create Cairo surface")?;
        let cr = CairoContext::new(&cairo_surface).context("Failed to create Cairo context")?;

        // Clear the background
        cr.set_source_rgb(0.2, 0.3, 0.8);
        cr.paint()?;

        // Get CPU load average
        let load_avg = fs::read_to_string("/proc/loadavg")?
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let end_angle = load_avg.min(1.0) * 2.0 * std::f64::consts::PI;

        // Draw the arc
        cr.set_source_rgb(0.0, 1.0, 0.0);
        cr.set_line_width(20.0);
        cr.arc(
            self.width as f64 / 2.0,
            self.height as f64 / 2.0,
            self.width.min(self.height) as f64 / 4.0,
            0.0,
            end_angle,
        );
        cr.stroke()?;

        // Display the load average
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        cr.set_font_size(24.0);
        
        let text = format!("Load Avg: {:.2}", load_avg);
        let extents = cr.text_extents(&text)?;
        let x = (self.width as f64 - extents.width()) / 2.0;
        let y = self.height as f64 / 2.0 + extents.height();
        
        cr.move_to(x, y);
        cr.show_text(&text)?;

        // Drop the Cairo context to release the surface
        drop(cr);

        // Get the surface data
        let data = cairo_surface.data().context("Failed to get Cairo surface data")?;
        
        // Create a shared memory buffer for Wayland
        let stride = (self.width * 4) as i32; // 4 bytes per pixel for ARGB32
        let size = stride * self.height as i32;
        
        // Create a temporary file for shared memory
        let temp_file = tempfile::tempfile().context("Failed to create temp file")?;
        temp_file.set_len(size as u64).context("Failed to set file size")?;
        
        // Map the file into memory
        let mut mmap = unsafe {
            memmap2::MmapOptions::new()
                .len(size as usize)
                .map_mut(&temp_file)
                .context("Failed to mmap file")?
        };
        
        // Copy Cairo surface data to shared memory
        mmap.copy_from_slice(&data);
        
        // Create shared memory pool
        let pool = shm.create_pool(
            unsafe { BorrowedFd::borrow_raw(temp_file.as_raw_fd()) },
            size,
            qhandle,
            (),
        );
        
        // Create buffer from the pool
        let buffer = pool.create_buffer(
            0,
            self.width as i32,
            self.height as i32,
            stride,
            wl_shm::Format::Argb8888,
            qhandle,
            (),
        );

        // Attach buffer to surface and commit
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        
        info!("Render completed successfully");
        Ok(())
    }
}

// Registry event handling
impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                info!("Global: {} {} {}", name, interface, version);
                
                match interface.as_str() {
                    "wl_compositor" => {
                        let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(name, version, qhandle, ());
                        state.compositor = Some(compositor);
                    }
                    "wl_shm" => {
                        let shm = registry.bind::<wl_shm::WlShm, _, _>(name, version, qhandle, ());
                        state.shm = Some(shm);
                    }
                    "zwlr_layer_shell_v1" => {
                        let layer_shell = registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(name, version, qhandle, ());
                        state.layer_shell = Some(layer_shell);
                    }
                    "xdg_wm_base" => {
                        let xdg_wm_base = registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, version, qhandle, ());
                        state.xdg_wm_base = Some(xdg_wm_base);
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                info!("Global removed: {}", name);
            }
            _ => {}
        }
    }
}

// Surface event handling
impl Dispatch<wl_surface::WlSurface, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Surface events
    }
}

// Compositor event handling
impl Dispatch<wl_compositor::WlCompositor, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Compositor events
    }
}

// Shm event handling
impl Dispatch<wl_shm::WlShm, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Shm events
    }
}

// Shm pool event handling
impl Dispatch<wl_shm_pool::WlShmPool, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Shm pool events
    }
}

// Buffer event handling
impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Buffer events
    }
}

// Layer shell event handling
impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // Layer shell events
    }
}

// XDG WM Base event handling
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &xdg_wm_base::XdgWmBase,
        _event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        // XDG WM Base events
    }
}

// Layer surface event handling
impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                info!("Layer surface configured: {}x{}", width, height);
                state.width = width;
                state.height = height;
                if let Some(layer_surface) = &state.layer_surface {
                    layer_surface.ack_configure(serial);
                }
                state.render(_qhandle).unwrap_or_else(|e| error!("Render error: {}", e));
            }
            zwlr_layer_surface_v1::Event::Closed => {
                info!("Layer surface closed");
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    env_logger::init();

    // Connect to Wayland
    let connection = Connection::connect_to_env().context("Failed to connect to Wayland")?;
    let mut event_queue = connection.new_event_queue();
    let qhandle = event_queue.handle();

    // Create application
    let mut app = App::new();

    // Get registry
    let _registry = connection.display().get_registry(&qhandle, ());

    // Initial roundtrip to get globals
    event_queue.roundtrip(&mut app).context("Failed to sync with compositor")?;

    // Create surface and layer surface if we have the required globals
    if let (Some(compositor), Some(layer_shell)) = (&app.compositor, &app.layer_shell) {
        let surface = compositor.create_surface(&qhandle, ());
        app.surface = Some(surface.clone());

        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Background,
            "widget".to_string(),
            &qhandle,
            (),
        );

        // Configure layer surface
        layer_surface.set_size(app.width, app.height);
        layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        layer_surface.set_exclusive_zone(0); // Don't reserve space, just show in background
        surface.commit();

        app.layer_surface = Some(layer_surface);

        // Do another roundtrip to get the configure event
        event_queue.roundtrip(&mut app).context("Failed to get layer surface configuration")?;
    } else {
        error!("Missing required Wayland globals");
        return Err(anyhow::anyhow!("Missing required Wayland globals"));
    }

    // Main event loop
    loop {
        event_queue.blocking_dispatch(&mut app)?;
    }

    Ok(())
}
