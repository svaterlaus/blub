use crate::{
    render_output::hdr_backbuffer::HdrBackbuffer,
    render_output::screen::Screen,
    wgpu_utils::uniformbuffer::PaddedVector3,
    wgpu_utils::{binding_builder::*, binding_glsl, pipelines::*, shader::ShaderDirectory, uniformbuffer::UniformBuffer},
};
use serde::Deserialize;
use std::{fs::File, io, io::BufReader, path::Path, rc::Rc};

// Data describing a scene.
#[derive(Deserialize)]
pub struct BackgroundConfig {
    pub dir_light_direction: cgmath::Vector3<f32>,
    pub dir_light_radiance: cgmath::Vector3<f32>,
    pub indirect_lighting_sh: [(f32, f32, f32); 9],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LightingAndBackgroundUniformBufferContent {
    pub dir_light_direction: PaddedVector3,
    pub dir_light_radiance: PaddedVector3,
    pub indirect_lighting_sh: [((f32, f32, f32), f32); 9],
}
unsafe impl bytemuck::Pod for LightingAndBackgroundUniformBufferContent {}
unsafe impl bytemuck::Zeroable for LightingAndBackgroundUniformBufferContent {}

type LightingAndBackgroundUniformBuffer = UniformBuffer<LightingAndBackgroundUniformBufferContent>;

pub struct Background {
    pipeline: RenderPipelineHandle,
    bind_group_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
}

mod cubemap_loader {
    use image::ImageDecoder;
    use image::codecs::hdr::HdrDecoder;
    use std::{
        fs::File,
        io::{Read, Write},
        path::{Path, PathBuf},
    };

    const CUBEMAP_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    fn get_cache_filename(path: &Path) -> PathBuf {
        path.join(".raw_rgba8_cubemap.cache")
    }

    fn linear_rgb_to_gamma_rgba8(r: f32, g: f32, b: f32) -> [u8; 4] {
        fn channel(x: f32) -> u8 {
            let x = x.max(0.0).powf(1.0 / 2.2).min(1.0);
            (x * 255.0).round() as u8
        }
        [channel(r), channel(g), channel(b), 255]
    }

    fn decode_hdr_face_rgba(reader: impl std::io::Read) -> Result<Vec<u8>, image::ImageError> {
        let decoder = HdrDecoder::new(reader)?;
        let dims = decoder.dimensions();
        if dims.0 != dims.1 {
            panic!("cubemap face width not equal height");
        }
        let mut linear = vec![0u8; decoder.total_bytes() as usize];
        decoder.read_image(&mut linear)?;
        let mut rgba = Vec::with_capacity(linear.len() / 3);
        for px in linear.chunks_exact(12) {
            let r = f32::from_ne_bytes(px[0..4].try_into().unwrap());
            let g = f32::from_ne_bytes(px[4..8].try_into().unwrap());
            let b = f32::from_ne_bytes(px[8..12].try_into().unwrap());
            rgba.extend_from_slice(&linear_rgb_to_gamma_rgba8(r, g, b));
        }
        Ok(rgba)
    }

    /// Cache layout: 6 faces × (`resolution`²) RGBA8 texels (`4 × 6 × resolution²` bytes).
    fn cube_face_resolution_from_raw_cache_len(bytes: usize) -> Option<u32> {
        if bytes == 0 || bytes % (4 * 6) != 0 {
            return None;
        }
        let pixels_per_face = bytes / 4 / 6;
        let r = (pixels_per_face as f64).sqrt().round() as u64;
        if r == 0 || r * r != pixels_per_face as u64 {
            return None;
        }
        Some(r as u32)
    }

    fn from_cache(path: &Path, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<wgpu::Texture, std::io::Error> {
        let cache_filename = get_cache_filename(path);
        info!("loading cubemap from cached raw file at {:?}", cache_filename);

        let mut image_data = Vec::new();
        File::open(&cache_filename)?.read_to_end(&mut image_data)?;
        let Some(resolution) = cube_face_resolution_from_raw_cache_len(image_data.len()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "cubemap cache has invalid size ({} bytes); expected 24 × (face resolution)²",
                    image_data.len()
                ),
            ));
        };

        let cubemap = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Cubemap"),
            size: wgpu::Extent3d {
                width: resolution,
                height: resolution,
                depth_or_array_layers: 6,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: CUBEMAP_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[CUBEMAP_FORMAT],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &cubemap,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image_data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * resolution),
                rows_per_image: Some(resolution),
            },
            wgpu::Extent3d {
                width: resolution,
                height: resolution,
                depth_or_array_layers: 6,
            },
        );

        Ok(cubemap)
    }

    // Loads cubemap from Radiance .hdr cube faces (decodes to gamma‑mapped RGBA8 for the RGBA8Unorm texture).
    fn from_hdr_faces(path: &Path, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<wgpu::Texture, std::io::Error> {
        let filenames = ["px.hdr", "nx.hdr", "py.hdr", "ny.hdr", "pz.hdr", "nz.hdr"];

        let mut cubemap = None;
        let mut resolution: u32 = 0;

        let mut cache_file = File::create(get_cache_filename(path)).unwrap();

        for (i, filename) in filenames.iter().enumerate() {
            info!("loading cubemap face {}..", i);

            let file_reader = std::io::BufReader::new(File::open(path.join(filename))?);
            let rgba = decode_hdr_face_rgba(file_reader).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}", e)))?;

            let face_res = ((rgba.len() / 4) as f64).sqrt() as u32;
            if face_res as u64 * face_res as u64 * 4 != rgba.len() as u64 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "hdr decode size mismatch"));
            }

            if cubemap.is_none() {
                resolution = face_res;
                cubemap = Some(device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("Cubemap"),
                    size: wgpu::Extent3d {
                        width: resolution,
                        height: resolution,
                        depth_or_array_layers: 6,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: CUBEMAP_FORMAT,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[CUBEMAP_FORMAT],
                }));
            }

            if resolution != face_res {
                panic!("all cubemap faces need to have the same resolution");
            }

            cache_file.write_all(&rgba)?;

            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: cubemap.as_ref().unwrap(),
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: i as u32 },
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * resolution),
                    rows_per_image: Some(resolution),
                },
                wgpu::Extent3d {
                    width: resolution,
                    height: resolution,
                    depth_or_array_layers: 1,
                },
            );
        }

        Ok(cubemap.unwrap())
    }

    pub fn load(path: &Path, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<wgpu::TextureView, std::io::Error> {
        // Loading .hdr is somewhat slow, especially so in debug. So we cache the raw data.
        let cubemap = match from_cache(path, device, queue) {
            Ok(cubemap) => cubemap,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("no cubemap cache, loading from .hdr faces");
                from_hdr_faces(path, device, queue)?
            }
            Err(e) => {
                warn!("{}", e);
                info!("loading cubemap from .hdr faces instead");
                from_hdr_faces(path, device, queue)?
            }
        };

        Ok(cubemap.create_view(&wgpu::TextureViewDescriptor {
            label: None,
            dimension: Some(wgpu::TextureViewDimension::Cube),
            ..wgpu::TextureViewDescriptor::default()
        }))
    }
}

impl Background {
    pub fn new(
        path: &Path,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        shader_dir: &ShaderDirectory,
        pipeline_manager: &mut PipelineManager,
        global_bind_group_layout: &wgpu::BindGroupLayout,
    ) -> Result<Self, io::Error> {
        let file = File::open(path.join("config.json"))?;
        let reader = BufReader::new(file);
        let config: BackgroundConfig = serde_json::from_reader(reader)?;

        let ubo = LightingAndBackgroundUniformBuffer::new_with_data(
            &device,
            &LightingAndBackgroundUniformBufferContent {
                dir_light_direction: config.dir_light_direction.into(),
                dir_light_radiance: config.dir_light_radiance.into(),
                indirect_lighting_sh: [
                    (config.indirect_lighting_sh[0], 0.0),
                    (config.indirect_lighting_sh[1], 0.0),
                    (config.indirect_lighting_sh[2], 0.0),
                    (config.indirect_lighting_sh[3], 0.0),
                    (config.indirect_lighting_sh[4], 0.0),
                    (config.indirect_lighting_sh[5], 0.0),
                    (config.indirect_lighting_sh[6], 0.0),
                    (config.indirect_lighting_sh[7], 0.0),
                    (config.indirect_lighting_sh[8], 0.0),
                ],
            },
        );

        let cubemap_view = cubemap_loader::load(path, device, queue)?;

        let bind_group_layout = BindGroupLayoutBuilder::new()
            .next_binding(wgpu::ShaderStages::COMPUTE | wgpu::ShaderStages::FRAGMENT, binding_glsl::uniform())
            .next_binding(wgpu::ShaderStages::COMPUTE | wgpu::ShaderStages::FRAGMENT, binding_glsl::textureCube())
            .create(device, "BindGroupLayout: Lighting & Background");

        let bind_group = BindGroupBuilder::new(&bind_group_layout)
            .resource(ubo.binding_resource())
            .texture(&cubemap_view)
            .create(device, "BindGroup: Lighting & Background");

        let mut render_pipeline_desc = RenderPipelineCreationDesc::new(
            "Cubemap Renderer",
            Rc::new(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Cubemap Renderer Pipeline Layout"),
                bind_group_layouts: &[Some(global_bind_group_layout), Some(&bind_group_layout.layout)],
                immediate_size: 0,
            })),
            Path::new("screentri.vert"),
            Path::new("background_render.frag"),
            HdrBackbuffer::FORMAT,
            None,
        );
        render_pipeline_desc.depth_stencil = Some(wgpu::DepthStencilState {
            format: Screen::FORMAT_DEPTH,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: Default::default(),
            bias: Default::default(),
        });

        Ok(Background {
            pipeline: pipeline_manager.create_render_pipeline(device, shader_dir, render_pipeline_desc),
            bind_group_layout: bind_group_layout.layout,
            bind_group,
        })
    }

    pub fn draw<'a>(&'a self, rpass: &mut wgpu::RenderPass<'a>, pipeline_manager: &'a PipelineManager) {
        rpass.set_bind_group(1, &self.bind_group, &[]);
        rpass.set_pipeline(pipeline_manager.get_render(&self.pipeline));
        rpass.draw(0..3, 0..1);
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.bind_group_layout
    }
}
