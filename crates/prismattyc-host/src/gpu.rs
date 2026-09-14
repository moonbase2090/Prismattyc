//! Feature-gated wgpu present of the existing CPU `u32` framebuffer.
//!
//! Glyphs stay on the CPU. This module only uploads the softbuffer-shaped
//! buffer and draws a nearest-neighbor fullscreen triangle. Init failure is
//! the caller's signal to fall back to softbuffer.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use winit::window::Window;

/// CPU pixel layout is `0x00RRGGBB` (same as softbuffer). Little-endian that
/// is B,G,R,0 — so a BGRA texture is a direct upload after setting alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadOrder {
    Bgra,
    /// Kept for the packing test and if the cpu texture ever becomes RGBA.
    #[allow(dead_code)]
    Rgba,
}

pub struct GpuPresent {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    cpu_tex: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    pixels: Vec<u32>,
    scratch: Vec<u8>,
    upload_order: UploadOrder,
}

impl GpuPresent {
    pub fn try_init(window: Arc<Window>) -> Result<Self> {
        let started = Instant::now();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| anyhow!("wgpu surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .context("no wgpu adapter (Vulkan/GL)")?;

        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("prismattyc-host"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| anyhow!("wgpu device: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        let format = pick_present_format(&caps.formats)
            .ok_or_else(|| anyhow!("no usable surface format in {:?}", caps.formats))?;
        let present_mode = pick_present_mode(&caps.present_modes);
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
            .or_else(|| caps.alpha_modes.first().copied())
            .unwrap_or(wgpu::CompositeAlphaMode::Auto);
        let size = window.inner_size();
        let width = size.width.max(1);
        let height = size.height.max(1);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("prismattyc-host present"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(PRESENT_WGSL)),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("prismattyc-host present"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("prismattyc-host present"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("prismattyc-host present"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let (cpu_tex, bind_group) = make_cpu_texture(&device, &bind_group_layout, width, height);
        let upload_order = upload_order_for(format);
        eprintln!(
            "prismattyc-host: GPU present {} / {:?} / {format:?} / {present_mode:?} in {:?}",
            info.name,
            info.backend,
            started.elapsed()
        );
        if !info.driver.is_empty() {
            eprintln!("prismattyc-host: GPU driver {}", info.driver);
        }

        Ok(Self {
            surface,
            device,
            queue,
            config,
            cpu_tex,
            bind_group,
            pipeline,
            bind_group_layout,
            pixels: vec![0; width as usize * height as usize],
            scratch: Vec::new(),
            upload_order,
        })
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    pub fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.max(1);
        let height = height.max(1);
        if self.config.width == width && self.config.height == height {
            return Ok(());
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        let (cpu_tex, bind_group) =
            make_cpu_texture(&self.device, &self.bind_group_layout, width, height);
        self.cpu_tex = cpu_tex;
        self.bind_group = bind_group;
        self.pixels
            .resize(width as usize * height as usize, 0x0012_1214);
        Ok(())
    }

    pub fn present(&mut self) -> Result<()> {
        prepare_upload(&mut self.pixels, self.upload_order);
        let width = self.config.width;
        let height = self.config.height;
        let src_stride = (width as usize).saturating_mul(4);
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let padded_stride = src_stride.div_ceil(align).saturating_mul(align).max(align);
        let bytes = if padded_stride == src_stride {
            u32_as_le_bytes(&self.pixels)
        } else {
            self.scratch
                .resize(padded_stride.saturating_mul(height as usize), 0);
            let packed = u32_as_le_bytes(&self.pixels);
            for y in 0..height as usize {
                let src = y.saturating_mul(src_stride);
                let dst = y.saturating_mul(padded_stride);
                if let (Some(src_row), Some(dst_row)) = (
                    packed.get(src..src.saturating_add(src_stride)),
                    self.scratch.get_mut(dst..dst.saturating_add(src_stride)),
                ) {
                    dst_row.copy_from_slice(src_row);
                }
            }
            self.scratch.as_slice()
        };

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.cpu_tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_stride as u32),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                self.surface
                    .get_current_texture()
                    .map_err(|e| anyhow!("wgpu surface after reconfigure: {e}"))?
            }
            Err(e) => return Err(anyhow!("wgpu surface: {e}")),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("prismattyc-host present"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("prismattyc-host present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        let _ = self.device.poll(wgpu::PollType::Poll);
        Ok(())
    }
}

fn make_cpu_texture(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    width: u32,
    height: u32,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("prismattyc-host cpu frame"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        // BGRA matches the LE layout of 0xFFRRGGBB. RGBA surfaces swizzle on upload.
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("prismattyc-host present"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&view),
        }],
    });
    (texture, bind_group)
}

fn pick_present_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    const PREFERRED: [wgpu::TextureFormat; 4] = [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::TextureFormat::Bgra8UnormSrgb,
        wgpu::TextureFormat::Rgba8UnormSrgb,
    ];
    for want in PREFERRED {
        if formats.contains(&want) {
            return Some(want);
        }
    }
    formats.first().copied()
}

fn pick_present_mode(modes: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    // On-demand present only — never pick a mode that implies a vsync poll loop.
    // We only submit when host.dirty. AutoNoVsync / Immediate avoid blocking the
    // event thread on the compositor clock.
    const PREFERRED: [wgpu::PresentMode; 4] = [
        wgpu::PresentMode::AutoNoVsync,
        wgpu::PresentMode::Immediate,
        wgpu::PresentMode::Mailbox,
        wgpu::PresentMode::Fifo,
    ];
    for want in PREFERRED {
        if modes.contains(&want) {
            return want;
        }
    }
    wgpu::PresentMode::Fifo
}

fn upload_order_for(_format: wgpu::TextureFormat) -> UploadOrder {
    // CPU texture is always Bgra8Unorm; 0xFFRRGGBB LE is already BGRA.
    UploadOrder::Bgra
}

fn prepare_upload(pixels: &mut [u32], order: UploadOrder) {
    match order {
        UploadOrder::Bgra => {
            for px in pixels {
                *px |= 0xFF00_0000;
            }
        }
        UploadOrder::Rgba => {
            for px in pixels {
                let r = (*px >> 16) & 0xFF;
                let g = (*px >> 8) & 0xFF;
                let b = *px & 0xFF;
                *px = r | (g << 8) | (b << 16) | 0xFF00_0000;
            }
        }
    }
}

fn u32_as_le_bytes(pixels: &[u32]) -> &[u8] {
    // u32 is a plain-old-data integer; the LE memory image is what wgpu wants.
    unsafe {
        std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), std::mem::size_of_val(pixels))
    }
}

const PRESENT_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var out: VsOut;
    out.pos = vec4<f32>(pos[i], 0.0, 1.0);
    out.uv = pos[i] * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5);
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(tex));
    let uv = clamp(in.uv, vec2<f32>(0.0), vec2<f32>(1.0) - vec2<f32>(1e-6));
    let px = vec2<i32>(uv * dims);
    return textureLoad(tex, px, 0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb_with_alpha_is_bgra_le_bytes() {
        let mut px = [0x00AA_BBCC];
        prepare_upload(&mut px, UploadOrder::Bgra);
        assert_eq!(px[0], 0xFFAA_BBCC);
        assert_eq!(px[0].to_le_bytes(), [0xCC, 0xBB, 0xAA, 0xFF]);
    }

    #[test]
    fn rgba_swizzle_is_rgba_le_bytes() {
        let mut px = [0x00AA_BBCC];
        prepare_upload(&mut px, UploadOrder::Rgba);
        assert_eq!(px[0].to_le_bytes(), [0xAA, 0xBB, 0xCC, 0xFF]);
    }

    #[test]
    fn present_mode_prefers_no_vsync() {
        assert_eq!(
            pick_present_mode(&[wgpu::PresentMode::Fifo, wgpu::PresentMode::AutoNoVsync]),
            wgpu::PresentMode::AutoNoVsync
        );
        assert_eq!(
            pick_present_mode(&[wgpu::PresentMode::Fifo]),
            wgpu::PresentMode::Fifo
        );
    }

    #[test]
    fn format_prefers_linear_bgra() {
        assert_eq!(
            pick_present_format(&[
                wgpu::TextureFormat::Bgra8UnormSrgb,
                wgpu::TextureFormat::Bgra8Unorm,
            ]),
            Some(wgpu::TextureFormat::Bgra8Unorm)
        );
    }

    #[test]
    fn instance_constructs_without_a_window() {
        // Compile + link proof. Adapter presence is machine-dependent; CI
        // has no GPU, so we only require that Instance::new does not panic.
        let _instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });
    }
}
