use crate::{
    config::settings,
    level,
    model,
    space::{Camera, Transform},
};

use cgmath::Decomposed;
use glsl_to_spirv;
use zerocopy::AsBytes as _;

use std::{
    io::{BufReader, Read, Write, Error as IoError},
    fs::File,
    mem,
    path::PathBuf,
    slice,
};

pub mod body;
pub mod collision;
pub mod debug;
pub mod global;
pub mod mipmap;
pub mod object;
pub mod terrain;


pub const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct GpuTransform {
    pub pos_scale: [f32; 4],
    pub orientation: [f32; 4],
}

impl GpuTransform {
    pub fn new(t: &Transform) -> Self {
        GpuTransform {
            pos_scale: [t.disp.x, t.disp.y, t.disp.z, t.scale],
            orientation: [t.rot.v.x, t.rot.v.y, t.rot.v.z, t.rot.s],
        }
    }
}

pub struct ScreenTargets<'a> {
    pub extent: wgpu::Extent3d,
    pub color: &'a wgpu::TextureView,
    pub depth: &'a wgpu::TextureView,
}

pub struct SurfaceData {
    pub constants: wgpu::Buffer,
    pub height: (wgpu::TextureView, wgpu::Sampler),
    pub meta: (wgpu::TextureView, wgpu::Sampler),
}

pub type ShapeVertex = [f32; 4];

#[repr(C)]
#[derive(Clone, Copy, zerocopy::AsBytes, zerocopy::FromBytes)]
pub struct ShapePolygon {
    pub indices: [u16; 4],
    pub normal: [i8; 4],
    pub origin_square: [f32; 4],
}

pub const SHAPE_POLYGON_BUFFER: wgpu::VertexBufferDescriptor  = wgpu::VertexBufferDescriptor {
    stride: mem::size_of::<ShapePolygon>() as wgpu::BufferAddress,
    step_mode: wgpu::InputStepMode::Instance,
    attributes: &[
        wgpu::VertexAttributeDescriptor {
            offset: 0,
            format: wgpu::VertexFormat::Ushort4,
            shader_location: 0,
        },
        wgpu::VertexAttributeDescriptor {
            offset: 8,
            format: wgpu::VertexFormat::Char4Norm,
            shader_location: 1,
        },
        wgpu::VertexAttributeDescriptor {
            offset: 12,
            format: wgpu::VertexFormat::Float4,
            shader_location: 2,
        },
    ],
};

pub struct Shaders {
    vs: wgpu::ShaderModule,
    fs: wgpu::ShaderModule,
}

impl Shaders {
    fn fail(name: &str, source: &str, log: &str) -> ! {
        println!("Generated shader:");
        for (i, line) in source.lines().enumerate() {
            println!("{:3}| {}", i+1, line);
        }
        let msg = log.replace("\\n", "\n");
        panic!("\nUnable to compile '{}': {}", name, msg);
    }

    pub fn new(
        name: &str,
        specialization: &[&str],
        device: &wgpu::Device,
    ) -> Result<Self, IoError> {
        let base_path = PathBuf::from("res").join("shader");
        let path = base_path.join(name).with_extension("glsl");
        if !path.is_file() {
            panic!("Shader not found: {:?}", path);
        }

        let mut buf_vs = b"#version 450\n#define SHADER_VS\n".to_vec();
        let mut buf_fs = b"#version 450\n#define SHADER_FS\n".to_vec();

        let mut code = String::new();
        BufReader::new(File::open(&path)?)
            .read_to_string(&mut code)?;
        // parse meta-data
        {
            let mut lines = code.lines();
            let first = lines.next().unwrap();
            if first.starts_with("//!include") {
                for include_pair in first.split_whitespace().skip(1) {
                    let mut temp = include_pair.split(':');
                    let target = match temp.next().unwrap() {
                        "vs" => &mut buf_vs,
                        "fs" => &mut buf_fs,
                        other => panic!("Unknown target: {}", other),
                    };
                    let include = temp.next().unwrap();
                    let inc_path = base_path
                        .join(include)
                        .with_extension("inc.glsl");
                    match File::open(&inc_path) {
                        Ok(include) => BufReader::new(include)
                            .read_to_end(target)?,
                        Err(e) => panic!("Unable to include {:?}: {:?}", inc_path, e),
                    };
                }
            }
            let second = lines.next().unwrap();
            if second.starts_with("//!specialization") {
                for define in second.split_whitespace().skip(1) {
                    let value = if specialization.contains(&define) {
                        1
                    } else {
                        0
                    };
                    write!(buf_vs, "#define {} {}\n", define, value)?;
                    write!(buf_fs, "#define {} {}\n", define, value)?;
                }
            }
        }

        write!(buf_vs, "\n{}", code
            .replace("attribute", "in")
            .replace("varying", "out")
        )?;
        write!(buf_fs, "\n{}", code
            .replace("varying", "in")
        )?;

        let str_vs = String::from_utf8_lossy(&buf_vs);
        let str_fs = String::from_utf8_lossy(&buf_fs);
        debug!("vs:\n{}", str_vs);
        debug!("fs:\n{}", str_fs);

        let spv_vs = match glsl_to_spirv::compile(&str_vs, glsl_to_spirv::ShaderType::Vertex) {
            Ok(file) => wgpu::read_spirv(file).unwrap(),
            Err(ref e) => {
                Self::fail(name, &str_vs, e);
            }
        };
        let spv_fs = match glsl_to_spirv::compile(&str_fs, glsl_to_spirv::ShaderType::Fragment) {
            Ok(file) => wgpu::read_spirv(file).unwrap(),
            Err(ref e) => {
                Self::fail(name, &str_fs, e);
            }
        };

        Ok(Shaders {
            vs: device.create_shader_module(&spv_vs),
            fs: device.create_shader_module(&spv_fs),
        })
    }

    pub fn new_compute(
        name: &str,
        group_size: [u32; 3],
        specialization: &[&str],
        device: &wgpu::Device,
    ) -> Result<wgpu::ShaderModule, IoError> {
        let base_path = PathBuf::from("res").join("shader");
        let path = base_path.join(name).with_extension("glsl");
        if !path.is_file() {
            panic!("Shader not found: {:?}", path);
        }

        let mut buf = b"#version 450\n".to_vec();
        write!(buf, "layout(local_size_x = {}, local_size_y = {}, local_size_z = {}) in;\n",
            group_size[0], group_size[1], group_size[2])?;
        write!(buf, "#define SHADER_CS\n")?;

        let mut code = String::new();
        BufReader::new(File::open(&path)?)
            .read_to_string(&mut code)?;
        // parse meta-data
        {
            let mut lines = code.lines();
            let first = lines.next().unwrap();
            if first.starts_with("//!include") {
                for include_pair in first.split_whitespace().skip(1) {
                    let mut temp = include_pair.split(':');
                    let target = match temp.next().unwrap() {
                        "cs" => &mut buf,
                        other => panic!("Unknown target: {}", other),
                    };
                    let include = temp.next().unwrap();
                    let inc_path = base_path
                        .join(include)
                        .with_extension("inc.glsl");
                    BufReader::new(File::open(inc_path)?)
                        .read_to_end(target)?;
                }
            }
            let second = lines.next().unwrap();
            if second.starts_with("//!specialization") {
                for define in second.split_whitespace().skip(1) {
                    let value = if specialization.contains(&define) {
                        1
                    } else {
                        0
                    };
                    write!(buf, "#define {} {}\n", define, value)?;
                }
            }
        }

        write!(buf, "\n{}", code)?;
        let str_cs = String::from_utf8_lossy(&buf);
        debug!("cs:\n{}", str_cs);

        let spv = match glsl_to_spirv::compile(&str_cs, glsl_to_spirv::ShaderType::Compute) {
            Ok(file) => wgpu::read_spirv(file).unwrap(),
            Err(ref e) => {
                Self::fail(name, &str_cs, e);
            }
        };

        Ok(device.create_shader_module(&spv))

    }
}


pub struct Palette {
    pub view: wgpu::TextureView,
}

impl Palette {
    pub fn new(
        encoder: &mut wgpu::CommandEncoder,
        device: &wgpu::Device,
        data: &[[u8; 4]],
    ) -> Self {
        let extent = wgpu::Extent3d {
            width: 0x100,
            height: 1,
            depth: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            size: extent,
            array_layer_count: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D1,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsage::SAMPLED | wgpu::TextureUsage::COPY_DST,
        });

        let staging = device.create_buffer_with_data(
            data.as_bytes(),
            wgpu::BufferUsage::COPY_SRC,
        );
        encoder.copy_buffer_to_texture(
            wgpu::BufferCopyView {
                buffer: &staging,
                offset: 0,
                row_pitch: 0x100 * 4,
                image_height: 1,
            },
            wgpu::TextureCopyView {
                texture: &texture,
                mip_level: 0,
                array_layer: 0,
                origin: wgpu::Origin3d {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
            },
            extent,
        );

        Palette {
            view: texture.create_default_view(),
        }
    }
}

pub fn instantiate_visual_model(
    model: &model::VisualModel,
    device: &wgpu::Device,
) -> wgpu::Buffer {
    let instance_size = mem::size_of::<object::Instance>() as wgpu::BufferAddress;
    device.create_buffer(&wgpu::BufferDescriptor {
        size: model.mesh_count() as wgpu::BufferAddress * instance_size,
        usage: wgpu::BufferUsage::VERTEX | wgpu::BufferUsage::COPY_DST,
    })
}


pub struct RenderModel<'a> {
    pub model: &'a model::VisualModel,
    pub gpu_body: &'a body::GpuBody,
    pub instance_buf: &'a wgpu::Buffer,
    pub transform: Transform,
    pub debug_shape_scale: Option<f32>,
}

impl RenderModel<'_> {
    /// Prepare the `instance_buf` data based on the transformation and
    /// model structure.
    ///
    /// For GPU physics path, needs to only be done once.
    /// For pure CPU path, needs to be done before every render.
    pub fn prepare(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        device: &wgpu::Device,
    ) {
        use cgmath::{Deg, One, Quaternion, Rad, Rotation3, Transform, Vector3};

        let count = self.model.mesh_count();
        let mapping = device.create_buffer_mapped(
            count * mem::size_of::<object::Instance>(),
            wgpu::BufferUsage::COPY_SRC,
        );
        {
            let data = unsafe {
                slice::from_raw_parts_mut(mapping.data.as_mut_ptr() as *mut object::Instance, count)
            };

            // body
            data[self.model.body.locals_id] = object::Instance::new(
                &self.transform,
                self.debug_shape_scale.unwrap_or_default(),
                self.gpu_body,
            );

            // wheels
            for w in self.model.wheels.iter() {
                if let Some(ref mesh) = w.mesh {
                    let transform = self.transform.concat(&Decomposed {
                        disp: mesh.offset.into(),
                        rot: Quaternion::one(),
                        scale: 1.0,
                    });
                    data[mesh.locals_id] = object::Instance::new(&transform, 0.0, self.gpu_body);
                }
            }
            // slots
            for s in self.model.slots.iter() {
                if let Some(ref mesh) = s.mesh {
                    let mut local = Decomposed {
                        disp: Vector3::new(s.pos[0] as f32, s.pos[1] as f32, s.pos[2] as f32),
                        rot: Quaternion::from_angle_y(Rad::from(Deg(s.angle as f32))),
                        scale: s.scale / self.transform.scale,
                    };
                    local.disp -= local.transform_vector(Vector3::from(mesh.offset));
                    let transform = self.transform.concat(&local);
                    data[mesh.locals_id] = object::Instance::new(&transform, 0.0, self.gpu_body);
                }
            }
        }

        let temp = mapping.finish();
        encoder.copy_buffer_to_buffer(
            &temp, 0,
            &self.instance_buf, 0,
            (count * mem::size_of::<object::Instance>()) as wgpu::BufferAddress,
        );
    }
}

struct InstanceArray {
    data: Vec<object::Instance>,
    // holding the mesh alive, while the key is just a raw pointer
    mesh: Arc<Mesh>,
}

pub struct Batcher {
    instances: HashMap<*const Mesh, InstanceArray>,
}

impl Batcher {
    fn add_mesh(&mut self, mesh: &Arc<Mesh>, instance: object::Instance) {
        self.instances
            .entry(mesh.as_ptr())
            .or_insert_with(|| InstanceArray {
                data: Vec::new(),
                mesh: Arc::clone(mesh),
            })
            .data.push(instance);
    }

    fn flush(&mut self, pass: &mut wgpu::RenderPass, device: &wgpu::Device) {
        for (_, array) in self.instances.drain() {
            let instance_buf = device.create_buffer_with_data(
                array.data.as_bytes(),
                wgpu::BufferUsage::VERTEX,
            );
            pass.set_vertex_buffers(0, &[(&array.mesh.vertex_buf, 0), (&instance_buf, 0)]);
            pass.draw(0 .. array.mesh.num_vertices as u32, 0 .. array.data.len() as u32);
        }
    }
}

pub struct Render {
    global: global::Context,
    pub object: object::Context,
    pub terrain: terrain::Context,
    pub debug: debug::Context,
    pub light_config: settings::Light,
}

impl Render {
    pub fn new(
        device: &wgpu::Device,
        queue: &mut wgpu::Queue,
        level: &level::Level,
        object_palette: &[[u8; 4]],
        settings: &settings::Render,
        screen_extent: wgpu::Extent3d,
        store_buffer: wgpu::BindingResource,
    ) -> Self {
        let mut init_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            todo: 0,
        });
        let global = global::Context::new(device, store_buffer);
        let object = object::Context::new(&mut init_encoder, device, object_palette, &global);
        let terrain = terrain::Context::new(&mut init_encoder, device, level, &global, &settings.terrain, screen_extent);
        let debug = debug::Context::new(device, &settings.debug, &global, &object);
        queue.submit(&[
            init_encoder.finish(),
        ]);

        Render {
            global,
            object,
            terrain,
            debug,
            light_config: settings.light.clone(),
        }
    }

    pub fn draw_mesh(
        pass: &mut wgpu::RenderPass,
        mesh: &model::Mesh,
        instance_buf: &wgpu::Buffer,
    ) {
        let locals_size = mem::size_of::<object::Instance>() as wgpu::BufferAddress;
        let offset = mesh.locals_id as wgpu::BufferAddress * locals_size;
        pass.set_vertex_buffers(0, &[(&mesh.vertex_buf, 0), (instance_buf, offset)]);
        pass.draw(0 .. mesh.num_vertices as u32, 0 .. 1);
    }

    pub fn draw_model(
        pass: &mut wgpu::RenderPass,
        model: &model::VisualModel,
        instance_buf: &wgpu::Buffer,
    ) {
        // body
        Render::draw_mesh(pass, &model.body, instance_buf);
        // wheels
        for w in model.wheels.iter() {
            if let Some(ref mesh) = w.mesh {
                Render::draw_mesh(pass, mesh, instance_buf);
            }
        }
        // slots
        for s in model.slots.iter() {
            if let Some(ref mesh) = s.mesh {
                Render::draw_mesh(pass, mesh, instance_buf);
            }
        }
    }

    pub fn draw_world<'a>(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        render_models: &[RenderModel<'a>],
        cam: &Camera,
        targets: ScreenTargets,
        device: &wgpu::Device,
    ) {
        let global_staging = device.create_buffer_with_data(
            global::Constants::new(cam, &self.light_config).as_bytes(),
            wgpu::BufferUsage::COPY_SRC,
        );
        encoder.copy_buffer_to_buffer(
            &global_staging,
            0,
            &self.global.uniform_buf,
            0,
            mem::size_of::<global::Constants>() as wgpu::BufferAddress,
        );

        self.terrain.prepare(encoder, device, &self.global, cam);

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[
                wgpu::RenderPassColorAttachmentDescriptor {
                    attachment: targets.color,
                    resolve_target: None,
                    load_op: wgpu::LoadOp::Clear,
                    store_op: wgpu::StoreOp::Store,
                    clear_color: wgpu::Color {
                        r: 0.1, g: 0.2, b: 0.3, a: 1.0,
                    },
                },
            ],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachmentDescriptor {
                attachment: targets.depth,
                depth_load_op: wgpu::LoadOp::Clear,
                depth_store_op: wgpu::StoreOp::Store,
                clear_depth: 1.0,
                stencil_load_op: wgpu::LoadOp::Clear,
                stencil_store_op: wgpu::StoreOp::Store,
                clear_stencil: 0,
            }),
        });

        pass.set_bind_group(0, &self.global.bind_group, &[]);
        self.terrain.draw(&mut pass);

        // draw vehicle models
        pass.set_pipeline(&self.object.pipeline);
        pass.set_bind_group(1, &self.object.bind_group, &[]);
        for rm in render_models {
            Render::draw_model(&mut pass, rm.model, rm.instance_buf);
        }

        // draw debug shapes
        for rm in render_models {
            self.debug.draw_shape(
                &mut pass,
                &rm.model.shape,
                rm.instance_buf,
            );
        }
    }

    pub fn reload(&mut self, device: &wgpu::Device) {
        info!("Reloading shaders");
        self.object.reload(device);
        self.terrain.reload(device);
    }

    pub fn resize(&mut self, extent: wgpu::Extent3d, device: &wgpu::Device) {
        self.terrain.resize(extent, device);
    }

    /*
    pub fn surface_data(&self) -> SurfaceData {
        SurfaceData {
            constants: self.terrain_data.suf_constants.clone(),
            height: self.terrain_data.height.clone(),
            meta: self.terrain_data.meta.clone(),
        }
    }*/

    /*
    pub fn target_color(&self) -> gfx::handle::RenderTargetView<R, ColorFormat> {
        self.terrain_data.out_color.clone()
    }*/
}
