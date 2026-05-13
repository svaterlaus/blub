use super::screenshot_capture::ScreenshotCapture;
use crate::wgpu_utils::binding_builder::*;
use crate::wgpu_utils::shader::*;
use crate::wgpu_utils::*;
use pipelines::*;
use std::{path::Path, rc::Rc};

pub struct Screen {
    resolution: winit::dpi::PhysicalSize<u32>,
    surface_config: wgpu::SurfaceConfiguration,
    present_mode: wgpu::PresentMode,

    backbuffer: wgpu::Texture,
    backbuffer_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,

    read_backbuffer_bind_group: wgpu::BindGroup,
    copy_to_swapchain_pipeline: RenderPipelineHandle,

    screenshot_capture: ScreenshotCapture,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct ScreenUniformBufferContent {
    resolution: cgmath::Point2<f32>,
    resolution_inv: cgmath::Point2<f32>,
}

impl Screen {
    pub const FORMAT_BACKBUFFER: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
    pub const FORMAT_DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
    pub const DEFAULT_PRESENT_MODE: wgpu::PresentMode = wgpu::PresentMode::Fifo;

    fn build_surface_config(surface: &wgpu::Surface, adapter: &wgpu::Adapter, present_mode: wgpu::PresentMode, resolution: winit::dpi::PhysicalSize<u32>) -> wgpu::SurfaceConfiguration {
        let caps = surface.get_capabilities(adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| matches!(f, wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb))
            .unwrap_or(caps.formats[0]);
        let present_mode = caps
            .present_modes
            .iter()
            .copied()
            .find(|&m| m == present_mode)
            .unwrap_or(caps.present_modes[0]);
        let alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|&m| m == wgpu::CompositeAlphaMode::Opaque)
            .unwrap_or(caps.alpha_modes[0]);
        wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: resolution.width.max(1),
            height: resolution.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        }
    }

    pub fn new(
        device: &wgpu::Device,
        surface: &wgpu::Surface<'_>,
        adapter: &wgpu::Adapter,
        present_mode: wgpu::PresentMode,
        resolution: winit::dpi::PhysicalSize<u32>,
        shader_dir: &ShaderDirectory,
        pipeline_manager: &mut PipelineManager,
    ) -> Self {
        info!("creating screen with {:?}", resolution);

        let surface_config = Self::build_surface_config(surface, adapter, present_mode, resolution);
        surface.configure(device, &surface_config);

        let size = wgpu::Extent3d {
            width: resolution.width.max(1),
            height: resolution.height.max(1),
            depth_or_array_layers: 1,
        };

        let backbuffer = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Texture: Backbuffer"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT_BACKBUFFER,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[Self::FORMAT_BACKBUFFER],
        });
        let backbuffer_view = backbuffer.create_view(&Default::default());

        let depth_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Texture: Screen DepthBuffer"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT_DEPTH,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[Self::FORMAT_DEPTH],
        });

        let bind_group_layout = BindGroupLayoutBuilder::new()
            .next_binding_fragment(binding_glsl::texture2D())
            .create(device, "BindGroupLayout: Screen, Read Texture");
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Screen Swapchain Copy Pipeline Layout"),
            bind_group_layouts: &[Some(&bind_group_layout.layout)],
            immediate_size: 0,
        });

        let read_backbuffer_bind_group = BindGroupBuilder::new(&bind_group_layout)
            .texture(&backbuffer_view)
            .create(device, "BindGroup: Read Backbuffer");

        let copy_to_swapchain_pipeline = pipeline_manager.create_render_pipeline(
            device,
            shader_dir,
            RenderPipelineCreationDesc::new(
                "Screen: Copy texture",
                Rc::new(pipeline_layout),
                Path::new("screentri.vert"),
                Path::new("copy_texture.frag"),
                surface_config.format,
                None,
            ),
        );

        Screen {
            resolution,
            surface_config,
            present_mode,
            backbuffer,
            backbuffer_view,
            depth_view: depth_texture.create_view(&Default::default()),

            read_backbuffer_bind_group,
            copy_to_swapchain_pipeline,
            screenshot_capture: ScreenshotCapture::new(device, resolution),
        }
    }

    pub fn aspect_ratio(&self) -> f32 {
        self.resolution.width as f32 / self.resolution.height as f32
    }

    pub fn resolution(&self) -> winit::dpi::PhysicalSize<u32> {
        self.resolution
    }

    pub fn backbuffer(&self) -> &wgpu::TextureView {
        &self.backbuffer_view
    }

    pub fn depthbuffer(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.present_mode
    }

    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.surface_config.format
    }

    pub fn configure_after_resize_or_present_change(
        &mut self,
        device: &wgpu::Device,
        surface: &wgpu::Surface<'_>,
        adapter: &wgpu::Adapter,
        present_mode: wgpu::PresentMode,
        resolution: winit::dpi::PhysicalSize<u32>,
    ) {
        self.present_mode = present_mode;
        self.surface_config = Self::build_surface_config(surface, adapter, present_mode, resolution);
        surface.configure(device, &self.surface_config);
    }

    pub fn capture_screenshot(&mut self, path: &Path, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder) {
        self.screenshot_capture.capture_screenshot(path, &self.backbuffer, device, encoder);
    }

    pub fn acquire_surface_texture(&mut self, device: &wgpu::Device, surface: &wgpu::Surface<'_>) -> Option<wgpu::SurfaceTexture> {
        loop {
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(frame) => return Some(frame),
                wgpu::CurrentSurfaceTexture::Suboptimal(frame) => return Some(frame),
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    info!(
                        "surface outdated or lost — reconfigure (resolution {:?}, present mode {:?})",
                        self.resolution, self.present_mode
                    );
                    surface.configure(device, &self.surface_config);
                }
                wgpu::CurrentSurfaceTexture::Timeout => {
                    return None;
                }
                wgpu::CurrentSurfaceTexture::Occluded => {
                    return None;
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    error!("surface get_current_texture validation error");
                    return None;
                }
            }
        }
    }

    pub fn copy_to_swapchain(&mut self, output: &wgpu::SurfaceTexture, encoder: &mut wgpu::CommandEncoder, pipeline_manager: &PipelineManager) {
        let swap_view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("copy to swapchain"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &swap_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        render_pass.set_pipeline(pipeline_manager.get_render(&self.copy_to_swapchain_pipeline));
        render_pass.set_bind_group(0, &self.read_backbuffer_bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }

    pub fn end_frame_present(&mut self, frame: wgpu::SurfaceTexture, device: &wgpu::Device) {
        frame.present();
        self.screenshot_capture.process_pending_screenshots(device);
    }

    pub fn wait_for_pending_screenshots(&mut self, device: &wgpu::Device) {
        self.screenshot_capture.wait_for_pending_screenshots(device);
    }

    pub fn fill_global_uniform_buffer(&self) -> ScreenUniformBufferContent {
        ScreenUniformBufferContent {
            resolution: cgmath::point2(self.resolution.width as f32, self.resolution.height as f32),
            resolution_inv: cgmath::point2(1.0 / self.resolution.width as f32, 1.0 / self.resolution.height as f32),
        }
    }
}
