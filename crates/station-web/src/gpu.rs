//! WebGPU renderer state and the per-frame geometry buffers.

use bytemuck::{Pod, Zeroable};
use std::f32::consts::PI;
use wasm_bindgen::JsValue;
#[cfg(target_arch = "wasm32")]
use web_sys::HtmlCanvasElement;

use crate::scene::{rotate_x, rotate_y, Plane, ProjectedNode, Vec2, Vec3};
use crate::util::Color;

#[cfg(target_arch = "wasm32")]
pub(crate) struct GpuState {
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) config: wgpu::SurfaceConfiguration,
    pub(crate) line_pipeline: wgpu::RenderPipeline,
    pub(crate) tri_pipeline: wgpu::RenderPipeline,
    /// Persistent vertex buffers, uploaded via `Queue::write_buffer` and
    /// grown geometrically on demand; never recreated per frame.
    pub(crate) line_buffer: GpuVertexBuffer,
    pub(crate) tri_buffer: GpuVertexBuffer,
}

#[cfg(target_arch = "wasm32")]
pub(crate) struct GpuVertexBuffer {
    pub(crate) label: &'static str,
    pub(crate) buffer: wgpu::Buffer,
    pub(crate) capacity: u64,
}

#[cfg(target_arch = "wasm32")]
impl GpuVertexBuffer {
    /// Comfortably holds a typical scene; grows if a frame outsizes it.
    pub(crate) const INITIAL_CAPACITY: u64 = 256 * 1024;

    pub(crate) fn new(device: &wgpu::Device, label: &'static str) -> Self {
        Self {
            label,
            buffer: Self::create(device, label, Self::INITIAL_CAPACITY),
            capacity: Self::INITIAL_CAPACITY,
        }
    }

    pub(crate) fn create(
        device: &wgpu::Device,
        label: &'static str,
        capacity: u64,
    ) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Upload this frame's vertices, growing the buffer if needed.
    pub(crate) fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        vertices: &[GpuVertex],
    ) {
        if vertices.is_empty() {
            return;
        }
        let bytes: &[u8] = bytemuck::cast_slice(vertices);
        let needed = bytes.len() as u64;
        if needed > self.capacity {
            self.capacity = needed.next_power_of_two();
            self.buffer = Self::create(device, self.label, self.capacity);
        }
        queue.write_buffer(&self.buffer, 0, bytes);
    }
}

#[cfg(target_arch = "wasm32")]
impl GpuState {
    pub(crate) async fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::BROWSER_WEBGPU;
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| JsValue::from_str(&format!("create WebGPU surface failed: {e:?}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("no WebGPU adapter available: {e:?}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("Intendant Station Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                ..Default::default()
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("request WebGPU device failed: {e:?}")))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // Shader/pipeline validation errors surface asynchronously on
        // WebGPU; without an error scope a broken shader yields pipelines
        // that silently no-op every render pass while init "succeeds".
        // Scope the whole pipeline setup so we fail loudly into the canvas
        // fallback instead.
        let error_scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Station Shader"),
            source: wgpu::ShaderSource::Wgsl(STATION_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Station Pipeline Layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let make_pipeline = |topology| {
            let vertex_layout = GpuVertex::layout();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Station Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[vertex_layout],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let line_pipeline = make_pipeline(wgpu::PrimitiveTopology::LineList);
        let tri_pipeline = make_pipeline(wgpu::PrimitiveTopology::TriangleList);
        if let Some(error) = error_scope.pop().await {
            return Err(JsValue::from_str(&format!(
                "WebGPU pipeline validation failed: {error}"
            )));
        }
        let line_buffer = GpuVertexBuffer::new(&device, "Station Lines");
        let tri_buffer = GpuVertexBuffer::new(&device, "Station Triangles");

        Ok(Self {
            surface,
            device,
            queue,
            config,
            line_pipeline,
            tri_pipeline,
            line_buffer,
            tri_buffer,
        })
    }

    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        if width == self.config.width && height == self.config.height {
            return;
        }
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub(crate) fn render(&mut self, frame: &GpuFrame) -> Result<(), JsValue> {
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output)
            | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(output)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
                    state => {
                        return Err(JsValue::from_str(&format!(
                            "surface unavailable after reconfigure: {state:?}"
                        )))
                    }
                }
            }
            state => {
                return Err(JsValue::from_str(&format!(
                    "surface unavailable: {state:?}"
                )))
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Station Encoder"),
            });

        self.line_buffer
            .upload(&self.device, &self.queue, &frame.line_vertices);
        self.tri_buffer
            .upload(&self.device, &self.queue, &frame.tri_vertices);

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Station Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.030,
                            g: 0.030,
                            b: 0.055,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !frame.line_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.line_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.line_pipeline);
                pass.set_vertex_buffer(0, self.line_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.line_vertices.len() as u32, 0..1);
            }
            if !frame.tri_vertices.is_empty() {
                let bytes = std::mem::size_of_val(frame.tri_vertices.as_slice()) as u64;
                pass.set_pipeline(&self.tri_pipeline);
                pass.set_vertex_buffer(0, self.tri_buffer.buffer.slice(..bytes));
                pass.draw(0..frame.tri_vertices.len() as u32, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct GpuState;

#[cfg(not(target_arch = "wasm32"))]
impl GpuState {
    pub(crate) fn resize(&mut self, _width: u32, _height: u32) {}

    pub(crate) fn render(&mut self, _frame: &GpuFrame) -> Result<(), JsValue> {
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
const STATION_WGSL: &str = r#"
struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
  var out: VertexOut;
  out.position = vec4<f32>(position, 0.0, 1.0);
  out.color = color;
  return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  return in.color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuVertex {
    pub(crate) pos: [f32; 2],
    pub(crate) color: [f32; 4],
}

impl GpuVertex {
    #[cfg(target_arch = "wasm32")]
    pub(crate) const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn layout<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

#[derive(Default)]
pub(crate) struct GpuFrame {
    pub(crate) line_vertices: Vec<GpuVertex>,
    pub(crate) tri_vertices: Vec<GpuVertex>,
    pub(crate) projected_nodes: Vec<ProjectedNode>,
}

impl GpuFrame {
    /// Empty the frame while keeping the buffers' capacity for reuse.
    pub(crate) fn clear(&mut self) {
        self.line_vertices.clear();
        self.tri_vertices.clear();
        self.projected_nodes.clear();
    }

    pub(crate) fn add_line_ndc(&mut self, a: Vec2, b: Vec2, color: Color) {
        self.line_vertices.push(GpuVertex {
            pos: [a.x, a.y],
            color: color.into(),
        });
        self.line_vertices.push(GpuVertex {
            pos: [b.x, b.y],
            color: color.into(),
        });
    }

    pub(crate) fn add_line_projected(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        a: Vec3,
        b: Vec3,
        color: Color,
    ) {
        if let (Some(pa), Some(pb)) = (project(a), project(b)) {
            self.add_line_ndc(pa, pb, color);
        }
    }

    pub(crate) fn add_quad_ndc(&mut self, x: f32, y: f32, size: f32, color: [f32; 4]) {
        let s = size;
        let verts = [
            [x - s, y - s],
            [x + s, y - s],
            [x + s, y + s],
            [x - s, y - s],
            [x + s, y + s],
            [x - s, y + s],
        ];
        for pos in verts {
            self.tri_vertices.push(GpuVertex { pos, color });
        }
    }

    pub(crate) fn add_ring(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        radius: f32,
        color: Color,
        plane: Plane,
    ) {
        let seg = 64;
        let mut prev = None;
        for i in 0..=seg {
            let t = i as f32 / seg as f32 * PI * 2.0;
            let local = match plane {
                Plane::XY => Vec3::new(t.cos() * radius, t.sin() * radius, 0.0),
                Plane::XZ => Vec3::new(t.cos() * radius, 0.0, t.sin() * radius),
                Plane::YZ => Vec3::new(0.0, t.cos() * radius, t.sin() * radius),
            };
            let p = center + local;
            if let Some(prev_p) = prev {
                self.add_line_projected(project, prev_p, p, color);
            }
            prev = Some(p);
        }
    }

    pub(crate) fn add_wire_octa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, -1.0, 0.0),
        ];
        let edges = [
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (5, 1),
            (5, 2),
            (5, 3),
            (5, 4),
            (1, 2),
            (2, 3),
            (3, 4),
            (4, 1),
        ];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    pub(crate) fn add_wire_tetra(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, -1.0, 1.0),
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(1.0, -1.0, -1.0),
        ];
        let edges = [(0, 1), (0, 2), (0, 3), (1, 2), (2, 3), (3, 1)];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    pub(crate) fn add_wire_icosa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let phi = 1.618;
        let verts = [
            Vec3::new(-1.0, phi, 0.0),
            Vec3::new(1.0, phi, 0.0),
            Vec3::new(-1.0, -phi, 0.0),
            Vec3::new(1.0, -phi, 0.0),
            Vec3::new(0.0, -1.0, phi),
            Vec3::new(0.0, 1.0, phi),
            Vec3::new(0.0, -1.0, -phi),
            Vec3::new(0.0, 1.0, -phi),
            Vec3::new(phi, 0.0, -1.0),
            Vec3::new(phi, 0.0, 1.0),
            Vec3::new(-phi, 0.0, -1.0),
            Vec3::new(-phi, 0.0, 1.0),
        ];
        let edges = [
            (0, 1),
            (0, 5),
            (0, 7),
            (0, 10),
            (0, 11),
            (1, 5),
            (1, 7),
            (1, 8),
            (1, 9),
            (2, 3),
            (2, 4),
            (2, 6),
            (2, 10),
            (2, 11),
            (3, 4),
            (3, 6),
            (3, 8),
            (3, 9),
            (4, 5),
            (4, 9),
            (4, 11),
            (5, 9),
            (5, 11),
            (6, 7),
            (6, 8),
            (6, 10),
            (7, 8),
            (7, 10),
            (8, 9),
            (10, 11),
        ];
        self.add_edges(project, center, scale * 0.55, spin, &verts, &edges, color);
    }

    pub(crate) fn add_wire_hex(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        radius: f32,
        height: f32,
        spin: f32,
        color: Color,
    ) {
        let mut top = Vec::with_capacity(6);
        let mut bottom = Vec::with_capacity(6);
        for i in 0..6 {
            let a = i as f32 / 6.0 * PI * 2.0 + spin;
            top.push(center + Vec3::new(a.cos() * radius, height * 0.5, a.sin() * radius));
            bottom.push(center + Vec3::new(a.cos() * radius, -height * 0.5, a.sin() * radius));
        }
        for i in 0..6 {
            let n = (i + 1) % 6;
            self.add_line_projected(project, top[i], top[n], color);
            self.add_line_projected(
                project,
                bottom[i],
                bottom[n],
                color.with_alpha(color.a * 0.7),
            );
            self.add_line_projected(project, top[i], bottom[i], color.with_alpha(color.a * 0.6));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_edges(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        verts: &[Vec3],
        edges: &[(usize, usize)],
        color: Color,
    ) {
        let transformed = verts
            .iter()
            .map(|v| center + rotate_y(rotate_x(*v * scale, spin * 0.7), spin))
            .collect::<Vec<_>>();
        for (a, b) in edges {
            self.add_line_projected(project, transformed[*a], transformed[*b], color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::Color;

    #[test]
    fn quads_push_two_triangles() {
        let mut frame = GpuFrame::default();
        frame.add_quad_ndc(0.0, 0.0, 0.1, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(frame.tri_vertices.len(), 6);
        assert!(frame.line_vertices.is_empty());
    }

    #[test]
    fn lines_push_vertex_pairs_and_projection_culls() {
        let mut frame = GpuFrame::default();
        let color = Color::rgb(255, 0, 0);
        frame.add_line_ndc(Vec2::new(0.0, 0.0), Vec2::new(1.0, 1.0), color);
        assert_eq!(frame.line_vertices.len(), 2);
        // A projector that culls everything adds nothing.
        let mut cull = |_: Vec3| -> Option<Vec2> { None };
        frame.add_line_projected(&mut cull, Vec3::ZERO, Vec3::Y, color);
        assert_eq!(frame.line_vertices.len(), 2);
    }

    #[test]
    fn ring_segments_share_endpoints() {
        let mut frame = GpuFrame::default();
        let mut identity = |v: Vec3| Some(Vec2::new(v.x, v.y));
        frame.add_ring(
            &mut identity,
            Vec3::ZERO,
            1.0,
            Color::rgb(0, 255, 0),
            Plane::XY,
        );
        // 64 segments, two vertices each.
        assert_eq!(frame.line_vertices.len(), 128);
    }

    #[test]
    fn clear_empties_but_keeps_capacity() {
        let mut frame = GpuFrame::default();
        frame.add_quad_ndc(0.0, 0.0, 0.1, [1.0; 4]);
        let cap = frame.tri_vertices.capacity();
        frame.clear();
        assert!(frame.tri_vertices.is_empty());
        assert!(frame.line_vertices.is_empty());
        assert!(frame.projected_nodes.is_empty());
        assert_eq!(frame.tri_vertices.capacity(), cap);
    }
}
