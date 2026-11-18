use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail, ensure};
use eframe::{
    egui, egui_glow,
    glow::{self, HasContext},
};
use oxideav_core::{ColorRange, PixelFormat};
use rvideoplayer::engine::Picture;

const VERTEX: &str = r#"
out vec2 uv;
void main() {
    vec2 point = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    uv = vec2(point.x, 1.0 - point.y);
    gl_Position = vec4(point * 2.0 - 1.0, 0., 1.);
}
"#;

const FRAGMENT: &str = r#"
in vec2 uv;
out vec4 out_color;
uniform sampler2D plane0;
uniform sampler2D plane1;
uniform sampler2D plane2;
uniform int layout_mode;
uniform int transfer;
uniform vec3 sample_scale;
uniform vec2 coefficients;
uniform vec4 range_values;

float sample_value(vec2 packed_sample, float scale) {
    return scale <= 1.0 ? packed_sample.r :
        (packed_sample.r + 256.0 * packed_sample.g) / scale;
}

vec3 to_sdr(vec3 signal) {
    signal = max(signal, vec3(0.0));
    vec3 light;
    if (transfer == 16) {
        vec3 x = pow(signal, vec3(1.0 / 78.84375));
        light = pow(max(x - 0.8359375, 0.0) / max(18.8515625 - 18.6875 * x, 0.001), vec3(1.0 / 0.1593017578)) * 100.0;
    } else {
        light = mix(signal * signal / 3.0,
            (exp((signal - 0.55991073) / 0.17883277) + 0.28466892) / 12.0,
            step(vec3(0.5), signal)) * 5.0;
    }
    vec3 rec709 = mat3(1.66049, -0.12455, -0.01815, -0.58764, 1.13290, -0.10058, -0.07285, -0.00835, 1.11873) * light;
    float peak = max(0.0, max(rec709.r, max(rec709.g, rec709.b)));
    return pow(clamp(rec709 / (1.0 + peak), 0.0, 1.0), vec3(1.0 / 2.2));
}

void main() {
    vec4 first = texture(plane0, uv);
    if (layout_mode == 4 || layout_mode == 5) {
        out_color = vec4(layout_mode == 4 ? first.rgb : first.bgr, 1.0);
        return;
    }

    vec2 second = texture(plane1, uv).rg;
    float luma = sample_value(first.rg, sample_scale.r);
    float u = sample_value(second, sample_scale.g);
    float v = sample_value(texture(plane2, uv).rg, sample_scale.b);

    if (layout_mode == 1 || layout_mode == 2) {
        u = layout_mode == 1 ? second.r : second.g;
        v = layout_mode == 1 ? second.g : second.r;
    }

    vec3 rgb;
    if (layout_mode == 3) {
        rgb = vec3(v, luma, u);
    } else {
        float y = (luma - range_values.x) * range_values.y;
        float cb = (u - range_values.z) * range_values.w;
        float cr = (v - range_values.z) * range_values.w;
        if (layout_mode == 6) {
            cb = 0.0;
            cr = 0.0;
        }
        float r = y + 2.0 * (1.0 - coefficients.x) * cr;
        float b = y + 2.0 * (1.0 - coefficients.y) * cb;
        float g = (y - coefficients.x * r - coefficients.y * b) / (1.0 - coefficients.x - coefficients.y);
        rgb = vec3(r,g,b);
    }
    if (transfer == 16 || transfer == 18) {
        rgb = to_sdr(rgb);
    }
    out_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

pub struct Renderer {
    program: glow::Program,
    vao: glow::VertexArray,
    textures: [glow::Texture; 3],
    geometry: [(u32, u32, usize); 3],
    uniforms: Uniforms,
    uploaded: Option<u64>,
    scratch: Vec<u8>,
    pub error: Option<String>,
}

struct Uniforms {
    layout: Option<glow::UniformLocation>,
    scale: Option<glow::UniformLocation>,
    range: Option<glow::UniformLocation>,
    coefficients: Option<glow::UniformLocation>,
    transfer: Option<glow::UniformLocation>,
}

pub fn paint(
    painter: &egui::Painter,
    rect: egui::Rect,
    renderer: Arc<Mutex<Renderer>>,
    picture: Arc<Picture>,
    serial: u64,
) {
    painter.add(egui::PaintCallback {
        rect,
        callback: Arc::new(egui_glow::CallbackFn::new(move |_, painter| {
            let mut render = renderer.lock().unwrap();
            if let Err(err) = render.draw(painter.gl(), &picture, serial) {
                render.error = Some(err.to_string());
            }
        })),
    });
}

impl Renderer {
    pub fn new(gl: &glow::Context) -> Result<Self> {
        // SAFETY: eframe has made this OpenGL context current on the UI thread.
        unsafe {
            let version = if gl.version().is_embedded {
                "#version 300 es\nprecision highp float;\n"
            } else {
                "#version 330 core\n"
            };
            let program = gl.create_program().map_err(|s| anyhow!(s))?;
            for (kind, source) in [
                (glow::VERTEX_SHADER, VERTEX),
                (glow::FRAGMENT_SHADER, FRAGMENT),
            ] {
                let shader = gl.create_shader(kind).map_err(|s| anyhow!(s))?;
                gl.shader_source(shader, &format!("{version}{source}"));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    let err = gl.get_shader_info_log(shader);
                    gl.delete_shader(shader);
                    gl.delete_program(program);
                    bail!("Video shader: {err}");
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                let err = gl.get_program_info_log(program);
                gl.delete_program(program);
                bail!("Video shader: {err}");
            }
            let vao = gl.create_vertex_array().map_err(|s| anyhow!(s))?;
            let textures = [
                gl.create_texture().map_err(|s| anyhow!(s))?,
                gl.create_texture().map_err(|s| anyhow!(s))?,
                gl.create_texture().map_err(|s| anyhow!(s))?,
            ];
            gl.use_program(Some(program));
            for (i, texture) in textures.into_iter().enumerate() {
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                for param in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, param, glow::LINEAR as i32);
                }
                for param in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, param, glow::CLAMP_TO_EDGE as i32);
                }
                gl.uniform_1_i32(
                    gl.get_uniform_location(program, &format!("plane{i}"))
                        .as_ref(),
                    i as i32,
                );
            }
            let uniform = |name| gl.get_uniform_location(program, name);
            let uniforms = Uniforms {
                layout: uniform("layout_mode"),
                scale: uniform("sample_scale"),
                range: uniform("range_values"),
                coefficients: uniform("coefficients"),
                transfer: uniform("transfer"),
            };
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
            Ok(Self {
                program,
                vao,
                textures,
                uniforms,
                geometry: [(0, 0, 0); 3],
                uploaded: None,
                scratch: Vec::new(),
                error: None,
            })
        }
    }

    fn draw(&mut self, gl: &glow::Context, picture: &Picture, serial: u64) -> Result<()> {
        // SAFETY: this method is called only by egui's active GL paint callback.
        unsafe { gl.use_program(Some(self.program)) };
        if self.uploaded != Some(serial) {
            self.upload(gl, picture)?;
            self.uploaded = Some(serial);
            self.error = None;
        }
        // SAFETY: eframe restores its bindings and state after the callback.
        unsafe {
            gl.disable(glow::BLEND);
            for (i, texture) in self.textures.into_iter().enumerate() {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            }
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.active_texture(glow::TEXTURE0);
        }
        Ok(())
    }

    fn upload(&mut self, gl: &glow::Context, picture: &Picture) -> Result<()> {
        use PixelFormat::*;
        let format = picture.format;
        let layout = match format {
            Nv12 => 1,
            Nv21 => 2,
            Gbrp8 | Gbrp10Le | Gbrp12Le | Gbrp14Le | Gbrp16Le => 3,
            Rgb24 | Rgba => 4,
            Bgr24 | Bgra => 5,
            Gray8 | Gray10Le | Gray12Le | Gray16Le => 6,
            _ if format.plane_count() >= 3 && format.chroma_subsampling().is_some() => {
                if picture.color.matrix.0 == 0 { 3 } else { 0 }
            }
            _ => bail!("Unsupported display pixel format {format:?}"),
        };
        let bits = match format {
            Yuv420P10Le | Yuv422P10Le | Yuv444P10Le | Gbrp10Le | Gray10Le => 10,
            Yuv420P12Le | Yuv422P12Le | Yuv444P12Le | Gbrp12Le | Gray12Le => 12,
            Gbrp14Le => 14,
            Yuv420P16Le | Yuv422P16Le | Yuv444P16Le | Gbrp16Le | Gray16Le => 16,
            _ => 8,
        };
        let planes = picture.frame.image_planes();
        let used = match layout {
            1 | 2 => 2,
            4..=6 => 1,
            _ => 3,
        };
        ensure!(planes.len() >= used, "Decoded frame has missing planes");
        let mut scale = [1.0; 3];
        for (i, plane) in planes.iter().take(used).enumerate() {
            let (w, h) = format
                .plane_dimensions(i, picture.width, picture.height)
                .context("Invalid plane dimensions")?;
            let bpp = match format {
                Rgb24 | Bgr24 => 3,
                Rgba | Bgra => 4,
                Nv12 | Nv21 if i == 1 => 2,
                _ if bits > 8 => 2,
                _ => 1,
            };
            let depth = picture
                .frame
                .plane_significant_bits(i)
                .unwrap_or(bits)
                .clamp(8, 16);
            if bits > 8 {
                scale[i] = ((1_u32 << depth) - 1) as f32 / 255.0;
            }
            let stride = plane.stride;
            let row = w as usize * bpp;
            ensure!(
                w > 0 && h > 0 && row <= stride,
                "Invalid decoded video plane"
            );
            let required = stride
                .checked_mul(h as usize - 1)
                .and_then(|n| n.checked_add(row))
                .context("Decoded video plane overflow")?;
            ensure!(
                plane.data.len() >= required,
                "Truncated decoded video plane"
            );
            let texels = if stride.is_multiple_of(bpp) {
                &plane.data[..required]
            } else {
                self.scratch.clear();
                for y in 0..h as usize {
                    self.scratch
                        .extend_from_slice(&plane.data[y * stride..y * stride + row]);
                }
                &self.scratch
            };
            let (internal, external) = [
                (glow::R8, glow::RED),
                (glow::RG8, glow::RG),
                (glow::RGB8, glow::RGB),
                (glow::RGBA8, glow::RGBA),
            ][bpp - 1];
            // SAFETY: dimensions, strides and the backing slice were validated above.
            unsafe {
                gl.active_texture(glow::TEXTURE0 + i as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(self.textures[i]));
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                gl.pixel_store_i32(
                    glow::UNPACK_ROW_LENGTH,
                    if stride.is_multiple_of(bpp) {
                        (stride / bpp) as i32
                    } else {
                        0
                    },
                );
                let pixels = glow::PixelUnpackData::Slice(Some(texels));
                if self.geometry[i] == (w, h, bpp) {
                    gl.tex_sub_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        0,
                        0,
                        w as i32,
                        h as i32,
                        external,
                        glow::UNSIGNED_BYTE,
                        pixels,
                    );
                } else {
                    gl.tex_image_2d(
                        glow::TEXTURE_2D,
                        0,
                        internal as i32,
                        w as i32,
                        h as i32,
                        0,
                        external,
                        glow::UNSIGNED_BYTE,
                        pixels,
                    );
                    self.geometry[i] = (w, h, bpp);
                }
                gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            }
        }

        let (kr, kb) = match picture.color.matrix.0 {
            1 => (0.2126, 0.0722),
            9 | 10 => (0.2627, 0.0593),
            5..=7 => (0.299, 0.114),
            _ if picture.height >= 720 => (0.2126, 0.0722),
            _ => (0.299, 0.114),
        };
        let depth = picture
            .frame
            .plane_significant_bits(0)
            .unwrap_or(bits)
            .clamp(8, 16);
        let codes = ((1_u32 << depth) - 1) as f32;
        let units = (1_u32 << (depth - 8)) as f32;
        let full = picture.color.range == ColorRange::Full
            || format.implied_color_range() == Some(ColorRange::Full);
        let (offset, gain, center, chroma_gain) = if full {
            (0.0, 1.0, 128.0 * units / codes, 1.0)
        } else {
            (
                16.0 * units / codes,
                codes / (219.0 * units),
                128.0 * units / codes,
                codes / (224.0 * units),
            )
        };
        // SAFETY: these uniform locations belong to the bound video shader.
        unsafe {
            gl.uniform_1_i32(self.uniforms.layout.as_ref(), layout);
            gl.uniform_1_i32(
                self.uniforms.transfer.as_ref(),
                i32::from(picture.color.transfer.0),
            );
            gl.uniform_3_f32_slice(self.uniforms.scale.as_ref(), &scale);
            gl.uniform_2_f32(self.uniforms.coefficients.as_ref(), kr, kb);
            gl.uniform_4_f32(
                self.uniforms.range.as_ref(),
                offset,
                gain,
                center,
                chroma_gain,
            );
        }
        Ok(())
    }

    pub fn destroy(&self, gl: &glow::Context) {
        // SAFETY: eframe calls on_exit while the GL context is current.
        unsafe {
            for tex in self.textures {
                gl.delete_texture(tex);
            }
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.program);
        }
    }
}
