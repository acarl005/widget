use std::{
    collections::VecDeque,
    f64::consts::PI,
    os::unix::io::{AsRawFd, BorrowedFd},
    path::Path,
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result};
use cairo::{FontSlant, FontWeight, Format, ImageSurface, LinearGradient};
use itertools::Itertools as _;
use log::{debug, error, info};
use sysinfo::{Disk, Disks, System};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::xdg::shell::client::xdg_wm_base;
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const RENDER_INTERVAL: Duration = Duration::from_millis(100);
// const RENDER_INTERVAL: Duration = Duration::from_secs(1);
const MAX_CPU_USAGE_POINTS: usize = 50;
const MAX_DISK_USAGE_POINTS: usize = 150;
const GAUGE_UPWARD_SHIFT: f64 = 20.;
const PILL_MARGIN: f64 = 20.;
const PILL_LENGTH: f64 = 175.;
const GRAPH_LENGTH: f64 = 175.;
const GRAPH_HEIGHT: f64 = 30.;
const GRAPH_BAR_WIDTH: f64 = GRAPH_LENGTH / MAX_DISK_USAGE_POINTS as f64;

struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    shm: Option<wl_shm::WlShm>,
    buffer_pool: Option<wl_shm_pool::WlShmPool>,
    buffer_file: Option<tempfile::NamedTempFile>,
    buffer_mmap: Option<memmap2::MmapMut>,
    buffer_size: usize,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    outputs: Vec<wl_output::WlOutput>,
    width: u32,
    height: u32,
    scale_factor: i32,
    system: System,
    disks: Disks,
    cpu_usage_points: VecDeque<f64>,
    read_bytes_points: VecDeque<u64>,
    written_bytes_points: VecDeque<u64>,
}

impl App {
    fn new() -> Self {
        let system = System::new();
        let disks = Disks::new_with_refreshed_list();

        let mut this = App {
            compositor: None,
            layer_shell: None,
            xdg_wm_base: None,
            shm: None,
            buffer_pool: None,
            buffer_file: None,
            buffer_mmap: None,
            buffer_size: 0,
            surface: None,
            layer_surface: None,
            outputs: Vec::new(),
            width: 0,        // Will be updated by layer surface configure event
            height: 0,       // Will be updated by layer surface configure event
            scale_factor: 1, // Will be updated from output events
            system,
            disks,
            cpu_usage_points: Default::default(),
            read_bytes_points: Default::default(),
            written_bytes_points: Default::default(),
        };
        this.refresh_system();
        this
    }

    fn refresh_system(&mut self) {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();
        self.disks.refresh(true /*remove_not_listed_disks*/);

        // Calculate average CPU usage across all cores
        let cpus = self.system.cpus();
        if cpus.is_empty() {
            panic!("CPUs cannot be empty");
        }
        let total_usage: f32 = cpus.iter().map(|cpu| cpu.cpu_usage()).sum();
        let cpu_usage = (total_usage / cpus.len() as f32).min(100.) as f64;
        push_within_limit(&mut self.cpu_usage_points, cpu_usage, MAX_CPU_USAGE_POINTS);

        let read_bytes = self
            .disks
            .iter()
            .map(|disk| disk.usage().read_bytes)
            .sum::<u64>();
        push_within_limit(
            &mut self.read_bytes_points,
            read_bytes,
            MAX_DISK_USAGE_POINTS,
        );

        let written_bytes = self
            .disks
            .iter()
            .map(|disk| disk.usage().written_bytes)
            .sum::<u64>();
        push_within_limit(
            &mut self.written_bytes_points,
            written_bytes,
            MAX_DISK_USAGE_POINTS,
        );
    }

    fn render(&mut self, qhandle: &QueueHandle<Self>) -> Result<()> {
        info!(
            "Render called with dimensions: {}x{}",
            self.width, self.height
        );

        if self.surface.is_none() || self.shm.is_none() {
            error!("Missing surface or shm in render");
            return Ok(());
        }

        self.refresh_system();

        // Create a Cairo surface scaled for high-DPI
        let physical_width = (self.width as i32) * self.scale_factor;
        let physical_height = (self.height as i32) * self.scale_factor;
        let mut cairo_surface =
            ImageSurface::create(Format::ARgb32, physical_width, physical_height)
                .context("Failed to create Cairo surface")?;
        let cairo_ctx =
            cairo::Context::new(&cairo_surface).context("Failed to create Cairo context")?;

        // Scale the Cairo context to work in logical coordinates
        cairo_ctx.scale(self.scale_factor as f64, self.scale_factor as f64);

        // Clear the background (transparent)
        cairo_ctx.set_source_rgba(0., 0., 0., 0.);
        cairo_ctx.set_operator(cairo::Operator::Source);
        cairo_ctx.paint().context("Failed to paint")?;
        cairo_ctx.set_operator(cairo::Operator::Over);

        self.draw_main(&cairo_ctx).context("Error in draw_main")?;

        // Drop the Cairo context to release the surface
        drop(cairo_ctx);

        // Get the surface data
        let data = cairo_surface
            .data()
            .context("Failed to get Cairo surface data")?;

        // Create a shared memory buffer for Wayland (using physical dimensions)
        let stride = physical_width * 4; // 4 bytes per pixel for ARGB32
        let size = (stride * physical_height) as usize;

        debug!(
            "Buffer calculation: stride={}, size={}, data.len()={}",
            stride,
            size,
            data.len()
        );

        // Check if we need to create new buffer or can reuse existing one
        if self.buffer_size != size || self.buffer_file.is_none() {
            info!(
                "Creating new buffer: old_size={}, new_size={}",
                self.buffer_size, size
            );

            // Create a new temporary file for shared memory
            let mut temp_file =
                tempfile::NamedTempFile::new().context("Failed to create temp file")?;
            temp_file
                .as_file_mut()
                .set_len(size as u64)
                .context("Failed to set file size")?;

            // Map the file into memory
            let mmap = unsafe {
                memmap2::MmapOptions::new()
                    .len(size)
                    .map_mut(temp_file.as_file())
                    .context("Failed to mmap file")?
            };

            // Create shared memory pool
            let shm = self.shm.as_ref().unwrap();
            let pool = shm.create_pool(
                unsafe { BorrowedFd::borrow_raw(temp_file.as_file().as_raw_fd()) },
                size as i32,
                qhandle,
                (),
            );

            // Store for reuse
            self.buffer_file = Some(temp_file);
            self.buffer_mmap = Some(mmap);
            self.buffer_pool = Some(pool);
            self.buffer_size = size;
        }

        let mmap = self.buffer_mmap.as_mut().unwrap();

        debug!(
            "About to copy {} bytes from Cairo surface data to mmap of len {}",
            data.len(),
            mmap.len()
        );

        // Copy Cairo surface data to shared memory
        mmap.copy_from_slice(&data);

        // Get the pool reference
        let pool = self.buffer_pool.as_ref().unwrap();

        // Create buffer from the pool (using physical dimensions)
        let buffer = pool.create_buffer(
            0,
            physical_width,
            physical_height,
            stride,
            wl_shm::Format::Argb8888,
            qhandle,
            (),
        );

        // Attach buffer to surface and commit
        let surface = self.surface.as_ref().unwrap();
        surface.set_buffer_scale(self.scale_factor);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();

        debug!("Render completed successfully");
        Ok(())
    }

    fn draw_main(&mut self, ctx: &cairo::Context) -> Result<()> {
        // Draw a circle with radial gradient at the bottom center
        let gauge_radius = 100.;
        let gauge_center_x = self.width as f64 / 2.;
        let gauge_center_y = self.height as f64 - GAUGE_UPWARD_SHIFT;

        let pattern = cairo::RadialGradient::new(
            gauge_center_x,
            gauge_center_y,
            0., // Inner circle (center, radius)
            gauge_center_x,
            gauge_center_y,
            gauge_radius, // Outer circle (center, radius)
        );

        pattern.add_color_stop_rgba(0., 0., 0., 0., 0.);
        pattern.add_color_stop_rgba(0.62, 0., 0., 0., 0.);
        pattern.add_color_stop_rgba(1., 208. / 255., 143. / 255., 1., 0.25);

        ctx.set_source(&pattern).context("Error setting pattern")?;
        ctx.arc(gauge_center_x, gauge_center_y, gauge_radius, 0., 2. * PI);
        ctx.fill()?;

        // Draw a border around it
        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.set_line_width(2.);
        ctx.arc(
            gauge_center_x,
            gauge_center_y,
            gauge_radius + 4.,
            0.,
            2. * PI,
        );
        ctx.stroke()?;

        let cpus = self.system.cpus();

        ctx.set_source_rgb(212. / 255., 79. / 255., 126. / 255.);
        ctx.set_line_width(4.);
        let top = 3. * PI / 2.;
        for (i, mut cpu_pair) in cpus.iter().chunks(2).into_iter().enumerate() {
            let Some(cpu1) = cpu_pair.next() else {
                continue;
            };
            let radius = gauge_radius - (i as f64) * 4. - 2.;
            ctx.arc(
                gauge_center_x,
                gauge_center_y,
                radius,
                top,
                top + (cpu1.cpu_usage() as f64) / 100. * PI / 2.,
            );
            ctx.stroke()?;

            if let Some(cpu2) = cpu_pair.next() {
                ctx.arc_negative(
                    gauge_center_x,
                    gauge_center_y,
                    radius,
                    top,
                    top - (cpu2.cpu_usage() as f64) / 100. * PI / 2.,
                );
                ctx.stroke()?;
            }
        }

        // Display the load average below the arc
        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.select_font_face("Inconsolata Nerd Font", FontSlant::Normal, FontWeight::Bold);
        ctx.set_font_size(16.);

        let text = format!("{:.1}%", self.cpu_usage_points.back().unwrap());
        let x = self.width as f64 / 2.;
        let y = self.height as f64 - 12.;
        self.text_centered_at(&text, x, y, 16., ctx)?;
        self.text_centered_at(" ", x, y - 24., 32., ctx)?;
        ctx.new_path();

        let arc_step = PI / MAX_CPU_USAGE_POINTS as f64;
        for (i, cpu_usage) in self.cpu_usage_points.iter().enumerate() {
            let line_width = *cpu_usage / 5.;
            ctx.set_line_width(line_width);
            ctx.arc_negative(
                gauge_center_x,
                gauge_center_y,
                gauge_radius + 6. + line_width / 2.,
                -arc_step * i as f64,
                -arc_step * i as f64 - arc_step,
            );
            ctx.set_source_rgb(212. / 255., 79. / 255., 126. / 255.);
            ctx.stroke()?;
        }

        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.set_line_width(1.);
        self.pill(
            gauge_center_x + gauge_radius + PILL_MARGIN,
            gauge_center_y - 2.,
            PILL_LENGTH,
            6.,
            ctx,
        )?;
        self.pill(
            gauge_center_x + gauge_radius + PILL_MARGIN,
            gauge_center_y + 10.,
            PILL_LENGTH,
            6.,
            ctx,
        )?;

        let root_partition = self
            .disks
            .iter()
            .find(|disk| disk.mount_point() == Path::new("/"))
            .expect("must have root partition");
        let root_partition_used = disk_used_frac(root_partition);

        ctx.set_line_cap(cairo::LineCap::Round);
        ctx.set_source_rgb(94. / 255., 1., 108. / 255.);
        ctx.move_to(
            gauge_center_x + gauge_radius + PILL_MARGIN,
            gauge_center_y + 1.,
        );
        ctx.rel_line_to(PILL_LENGTH * root_partition_used, 0.);
        ctx.stroke()?;

        let boot_partition = self
            .disks
            .iter()
            .find(|disk| disk.mount_point() == Path::new("/boot/efi/"))
            .expect("must have boot partition");
        let boot_partition_used = disk_used_frac(boot_partition);

        ctx.set_source_rgb(212. / 255., 79. / 255., 126. / 255.);
        ctx.move_to(
            gauge_center_x + gauge_radius + PILL_MARGIN,
            gauge_center_y + 13.,
        );
        ctx.rel_line_to(PILL_LENGTH * boot_partition_used, 0.);
        ctx.stroke()?;

        let rect_origin_x = gauge_center_x + gauge_radius + PILL_LENGTH + PILL_MARGIN * 2.;
        let rect_origin_y = gauge_center_y - 7.;
        let rect_size_x = 15.;
        let rect_size_y = self.height as f64 - rect_origin_y;
        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.move_to(rect_origin_x - 2., rect_origin_y);
        ctx.rel_line_to(0., rect_size_y);
        ctx.stroke()?;

        let pattern = LinearGradient::new(
            rect_origin_x,
            rect_origin_y,
            rect_origin_x + rect_size_x,
            rect_origin_y,
        );
        pattern.add_color_stop_rgba(0., 208. / 255., 143. / 255., 1., 0.25);
        pattern.add_color_stop_rgba(1., 0., 0., 0., 0.);
        ctx.rectangle(rect_origin_x, rect_origin_y, rect_size_x, rect_size_y);
        ctx.set_source(pattern)?;
        ctx.fill()?;

        let text_x = rect_origin_x + 10.;
        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.set_font_size(32.);
        ctx.move_to(text_x, rect_origin_y - 12.);
        ctx.show_text("󰋊 ")?;

        ctx.set_font_size(10.);
        ctx.move_to(text_x, rect_origin_y + 10.);
        ctx.show_text(&format!(
            "{:.1}% {}",
            root_partition_used * 100.,
            root_partition.mount_point().display()
        ))?;
        ctx.move_to(text_x, rect_origin_y + 22.);
        ctx.show_text(&format!(
            "{:.1}% {}",
            boot_partition_used * 100.,
            boot_partition.mount_point().display()
        ))?;

        ctx.move_to(text_x + 100., rect_origin_y + 10.);
        ctx.show_text(&format!(
            "  {}",
            format_bytes(
                self.disks
                    .iter()
                    .map(|disk| disk.usage().read_bytes)
                    .sum::<u64>(),
            )
        ))?;
        ctx.move_to(text_x + 100., rect_origin_y + 22.);
        ctx.show_text(&format!(
            "  {}",
            format_bytes(
                self.disks
                    .iter()
                    .map(|disk| disk.usage().written_bytes)
                    .sum::<u64>(),
            )
        ))?;

        let rect_origin_x = text_x + 150.;
        let pattern = LinearGradient::new(
            rect_origin_x,
            rect_origin_y,
            rect_origin_x + rect_size_x,
            rect_origin_y,
        );
        pattern.add_color_stop_rgba(0., 0., 0., 0., 0.);
        pattern.add_color_stop_rgba(1., 208. / 255., 143. / 255., 1., 0.25);
        ctx.rectangle(rect_origin_x, rect_origin_y, rect_size_x, rect_size_y);
        ctx.set_source(pattern)?;
        ctx.fill()?;

        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.move_to(rect_origin_x + rect_size_x + 2., rect_origin_y);
        ctx.rel_line_to(0., rect_size_y);
        ctx.stroke()?;

        ctx.set_source_rgb(212. / 255., 79. / 255., 126. / 255.);
        let read_bytes_max_val = 1.0f64.max(*self.read_bytes_points.iter().max().unwrap() as f64);
        for (i, read_bytes_point) in self.read_bytes_points.iter().enumerate() {
            let rect_height = *read_bytes_point as f64 / read_bytes_max_val * GRAPH_HEIGHT;
            ctx.rectangle(
                rect_origin_x + rect_size_x + 3. + GRAPH_LENGTH - i as f64 * GRAPH_BAR_WIDTH,
                self.height as f64 - rect_height,
                GRAPH_BAR_WIDTH,
                rect_height,
            );
            ctx.fill()?;
        }

        ctx.set_source_rgb(94. / 255., 1., 108. / 255.);
        let written_bytes_max_val =
            1.0f64.max(*self.written_bytes_points.iter().max().unwrap() as f64);
        for (i, written_bytes_point) in self.written_bytes_points.iter().enumerate() {
            let rect_height = *written_bytes_point as f64 / written_bytes_max_val * GRAPH_HEIGHT;
            ctx.rectangle(
                rect_origin_x + rect_size_x + 3. + GRAPH_LENGTH - i as f64 * GRAPH_BAR_WIDTH,
                self.height as f64 - rect_height,
                GRAPH_BAR_WIDTH,
                rect_height,
            );
            ctx.fill()?;
        }

        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.set_line_width(1.);
        self.pill(
            gauge_center_x - gauge_radius - PILL_MARGIN - PILL_LENGTH,
            gauge_center_y - 2.,
            PILL_LENGTH,
            6.,
            ctx,
        )?;
        self.pill(
            gauge_center_x - gauge_radius - PILL_MARGIN - PILL_LENGTH,
            gauge_center_y + 10.,
            PILL_LENGTH,
            6.,
            ctx,
        )?;

        let frac_swap_used = self.system.used_swap() as f64 / self.system.total_swap() as f64;
        ctx.set_line_cap(cairo::LineCap::Round);
        ctx.set_source_rgb(94. / 255., 1., 108. / 255.);
        ctx.move_to(
            gauge_center_x - gauge_radius - PILL_MARGIN,
            gauge_center_y + 1.,
        );
        ctx.rel_line_to(-PILL_LENGTH * frac_swap_used, 0.);
        ctx.stroke()?;

        let frac_mem_used = self.system.used_memory() as f64 / self.system.total_memory() as f64;
        ctx.set_source_rgb(212. / 255., 79. / 255., 126. / 255.);
        ctx.move_to(
            gauge_center_x - gauge_radius - PILL_MARGIN,
            gauge_center_y + 13.,
        );
        ctx.rel_line_to(-PILL_LENGTH * frac_mem_used, 0.);
        ctx.stroke()?;

        let rect_size_x = 15.;
        let rect_origin_y = gauge_center_y - 7.;
        let rect_size_y = self.height as f64 - rect_origin_y;
        let rect_origin_x =
            gauge_center_x - gauge_radius - PILL_LENGTH - PILL_MARGIN * 2. - rect_size_x;
        ctx.set_source_rgba(1., 1., 1., 0.6);
        ctx.move_to(rect_origin_x + rect_size_x + 2., rect_origin_y);
        ctx.rel_line_to(0., rect_size_y);
        ctx.stroke()?;

        let pattern = LinearGradient::new(
            rect_origin_x,
            rect_origin_y,
            rect_origin_x + rect_size_x,
            rect_origin_y,
        );
        pattern.add_color_stop_rgba(0., 0., 0., 0., 0.);
        pattern.add_color_stop_rgba(1., 208. / 255., 143. / 255., 1., 0.25);
        ctx.rectangle(rect_origin_x, rect_origin_y, rect_size_x, rect_size_y);
        ctx.set_source(pattern)?;
        ctx.fill()?;

        Ok(())
    }

    fn pill(
        &self,
        origin_x: f64,
        origin_y: f64,
        size_x: f64,
        size_y: f64,
        ctx: &cairo::Context,
    ) -> Result<()> {
        let radius = size_y / 2.;
        ctx.move_to(origin_x, origin_y);
        ctx.rel_line_to(size_x, 0.);
        let (curr_x, curr_y) = ctx.current_point()?;
        ctx.arc(curr_x, curr_y + radius, radius, 3. * PI / 2., PI / 2.);
        ctx.rel_line_to(-PILL_LENGTH, 0.);
        let (curr_x, curr_y) = ctx.current_point()?;
        ctx.arc(curr_x, curr_y - radius, radius, PI / 2., 3. * PI / 2.);
        ctx.stroke()?;
        Ok(())
    }

    fn text_centered_at(
        &self,
        text: &str,
        x: f64,
        y: f64,
        font_size: f64,
        ctx: &cairo::Context,
    ) -> Result<()> {
        ctx.set_font_size(font_size);
        let extents = ctx.text_extents(text)?;
        let x = x - (extents.width() / 2.);
        ctx.move_to(x, y);
        ctx.show_text(text)?;
        Ok(())
    }
}

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
                        let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(
                            name,
                            version,
                            qhandle,
                            (),
                        );
                        state.compositor = Some(compositor);
                    }
                    "wl_shm" => {
                        let shm = registry.bind::<wl_shm::WlShm, _, _>(name, version, qhandle, ());
                        state.shm = Some(shm);
                    }
                    "wl_output" => {
                        let output =
                            registry.bind::<wl_output::WlOutput, _, _>(name, version, qhandle, ());
                        state.outputs.push(output);
                    }
                    "zwlr_layer_shell_v1" => {
                        let layer_shell = registry
                            .bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                                name,
                                version,
                                qhandle,
                                (),
                            );
                        state.layer_shell = Some(layer_shell);
                    }
                    "xdg_wm_base" => {
                        let xdg_wm_base = registry.bind::<xdg_wm_base::XdgWmBase, _, _>(
                            name,
                            version,
                            qhandle,
                            (),
                        );
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

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            debug!("Frame callback done - triggering render");
            if let Err(e) = state.render(qhandle) {
                error!("Frame callback render error: {}", e);
            }

            // Schedule next frame callback after a 1-second delay
            thread::sleep(RENDER_INTERVAL);
            if let Some(surface) = &state.surface {
                let _callback = surface.frame(qhandle, ());
            }
        }
    }
}

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

impl Dispatch<wl_output::WlOutput, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            info!("Output scale factor: {}", factor);
            state.scale_factor = factor;
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                info!("Layer surface configured: {}x{}", width, height);
                state.width = width;
                state.height = height;
                if let Some(layer_surface) = &state.layer_surface {
                    layer_surface.ack_configure(serial);
                }
                state
                    .render(qhandle)
                    .unwrap_or_else(|e| error!("Render error: {}", e));

                // Schedule a frame callback to trigger periodic updates
                if let Some(surface) = &state.surface {
                    // The callback will trigger in the frame callback handler
                    let _callback = surface.frame(qhandle, ());
                }
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

    let connection = Connection::connect_to_env().context("Failed to connect to Wayland")?;
    let mut event_queue = connection.new_event_queue();
    let qhandle = event_queue.handle();

    let mut app = App::new();

    let _registry = connection.display().get_registry(&qhandle, ());

    // Initial roundtrip to get globals
    event_queue
        .roundtrip(&mut app)
        .context("Failed to sync with compositor")?;

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

        // Configure layer surface to span the whole screen. 0 means use full screen size
        layer_surface.set_size(0, 0);
        layer_surface.set_anchor(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Bottom
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        ); // Anchor to all edges to fill the screen
        layer_surface.set_exclusive_zone(0); // Don't reserve space, just show in background
        surface.commit();

        app.layer_surface = Some(layer_surface);

        // Do another roundtrip to get the configure event
        event_queue
            .roundtrip(&mut app)
            .context("Failed to get layer surface configuration")?;
    } else {
        error!("Missing required Wayland globals");
        return Err(anyhow::anyhow!("Missing required Wayland globals"));
    }

    loop {
        event_queue.blocking_dispatch(&mut app)?;
    }
}

fn disk_used_frac(disk: &Disk) -> f64 {
    1. - (disk.total_space() - disk.available_space()) as f64 / disk.total_space() as f64
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes}B");
    }
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    let mut val = bytes as f64;
    for unit in UNITS {
        val /= 1024.;
        if val < 1000. {
            return format!("{val:.1}{unit}");
        }
    }
    format!("{val:.1}PB")
}

fn push_within_limit<T>(values: &mut VecDeque<T>, new_value: T, limit: usize) -> Option<T> {
    values.push_front(new_value);
    if values.len() > limit {
        values.pop_back()
    } else {
        None
    }
}

mod tests {
    #[test]
    fn test_format_bytes() {
        use super::format_bytes;

        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(43), "43B");
        assert_eq!(format_bytes(999), "999B");
        assert_eq!(format_bytes(1000), "1.0kB");
        assert_eq!(format_bytes(1076), "1.1kB");
        assert_eq!(format_bytes(1048574), "1.0MB");
        assert_eq!(format_bytes(1048578), "1.0MB");
        assert_eq!(format_bytes(16043212), "15.3MB");
        assert_eq!(format_bytes(702227152896), "654.0GB");
        assert_eq!(format_bytes(1039475162591213420), "923.2PB");
        assert_eq!(format_bytes(1503947516259121342), "1335.8PB");
    }
}
