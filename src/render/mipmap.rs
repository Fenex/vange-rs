use crate::render::{
    terrain::{Rect, HEIGHT_FORMAT},
    Shaders,
};
use bytemuck::{Pod, Zeroable};
use std::{mem, num::NonZeroU32};
use wgpu::util::DeviceExt as _;

#[repr(C)]
#[derive(Clone, Copy)]
struct Vertex {
    _pos: [f32; 2],
}
unsafe impl Pod for Vertex {}
unsafe impl Zeroable for Vertex {}

struct Mip {
    view: wgpu::TextureView,
    bind_group: wgpu::BindGroup,
}

pub struct MaxMipper {
    size: wgpu::Extent3d,
    pipeline_layout: wgpu::PipelineLayout,
    pipeline: wgpu::RenderPipeline,
    //data: terrain_mip::Data<R>,
    mips: Vec<Mip>,
}

impl MaxMipper {
    fn create_pipeline(
        layout: &wgpu::PipelineLayout,
        device: &wgpu::Device,
    ) -> wgpu::RenderPipeline {
        let shaders = Shaders::new("terrain/mip", &[], device).unwrap();
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mipmap"),
            layout: Some(layout),
            vertex_stage: wgpu::ProgrammableStageDescriptor {
                module: &shaders.vs,
                entry_point: "main",
            },
            fragment_stage: Some(wgpu::ProgrammableStageDescriptor {
                module: &shaders.fs,
                entry_point: "main",
            }),
            rasterization_state: Some(wgpu::RasterizationStateDescriptor {
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: wgpu::CullMode::None,
                ..Default::default()
            }),
            primitive_topology: wgpu::PrimitiveTopology::TriangleList,
            color_states: &[HEIGHT_FORMAT.into()],
            depth_stencil_state: None,
            vertex_state: wgpu::VertexStateDescriptor {
                index_format: wgpu::IndexFormat::Uint16,
                vertex_buffers: &[wgpu::VertexBufferDescriptor {
                    stride: mem::size_of::<Vertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::InputStepMode::Vertex,
                    attributes: &[wgpu::VertexAttributeDescriptor {
                        offset: 0,
                        format: wgpu::VertexFormat::Float2,
                        shader_location: 0,
                    }],
                }],
            },
            sample_count: 1,
            alpha_to_coverage_enabled: false,
            sample_mask: !0,
        })
    }

    pub fn new(
        texture: &wgpu::Texture,
        size: wgpu::Extent3d,
        mip_count: u32,
        device: &wgpu::Device,
    ) -> Self {
        let bg_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("MaxMipper"),
            entries: &[
                // sampler
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStage::FRAGMENT,
                    ty: wgpu::BindingType::Sampler { comparison: false },
                    count: None,
                },
                // texture
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStage::FRAGMENT,
                    ty: wgpu::BindingType::SampledTexture {
                        dimension: wgpu::TextureViewDimension::D2,
                        component_type: wgpu::TextureComponentType::Float,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mipmap"),
            bind_group_layouts: &[&bg_layout],
            push_constant_ranges: &[],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let mut mips = Vec::with_capacity(mip_count as usize);
        for level in 0..mip_count {
            let view = texture.create_view(&wgpu::TextureViewDescriptor {
                label: None,
                format: None,
                dimension: None,
                aspect: wgpu::TextureAspect::All,
                base_mip_level: level,
                level_count: NonZeroU32::new(1),
                base_array_layer: 0,
                array_layer_count: NonZeroU32::new(1),
            });

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("MaxMipper"),
                layout: &bg_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                ],
            });

            mips.push(Mip { view, bind_group });
        }

        let pipeline = Self::create_pipeline(&pipeline_layout, device);

        MaxMipper {
            size,
            pipeline_layout,
            pipeline,
            mips,
        }
    }

    pub fn update(
        &self,
        rects: &[Rect],
        encoder: &mut wgpu::CommandEncoder,
        device: &wgpu::Device,
    ) {
        let mut vertex_data = Vec::with_capacity(rects.len() * 6);
        for r in rects.iter() {
            let v_abs = [
                (r.x, r.y),
                (r.x + r.w, r.y),
                (r.x, r.y + r.h),
                (r.x, r.y + r.h),
                (r.x + r.w, r.y),
                (r.x + r.w, r.y + r.h),
            ];
            for &(x, y) in v_abs.iter() {
                vertex_data.push(Vertex {
                    _pos: [
                        x as f32 / self.size.width as f32,
                        y as f32 / self.size.height as f32,
                    ],
                });
            }
        }
        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mipmap-vertex"),
            contents: bytemuck::cast_slice(&vertex_data),
            usage: wgpu::BufferUsage::VERTEX,
        });

        for mip in 0..self.mips.len() - 1 {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &[wgpu::RenderPassColorAttachmentDescriptor {
                    attachment: &self.mips[mip + 1].view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.mips[mip].bind_group, &[]);
            pass.set_vertex_buffer(0, vertex_buf.slice(..));
            pass.draw(0..rects.len() as u32 * 6, 0..1);
        }
    }

    pub fn reload(&mut self, device: &wgpu::Device) {
        self.pipeline = Self::create_pipeline(&self.pipeline_layout, device);
    }
}
