//! GPU-accelerated terminal rendering: a glyph atlas (each unique glyph's
//! coverage rasterized once via `fontdue`, uploaded once) plus instanced
//! quads (one draw call per frame for backgrounds, one for glyphs),
//! replacing the old per-pixel CPU blitting in `mudhuts_term::render`.
//! See the Phase 2.6 plan notes for why: `btop` alone measured 60% of one
//! CPU core in ~1/10th of a 4K screen with the CPU path, which doesn't
//! scale to a full 4K120Hz screen.
//!
//! Uses `GlesRenderer::with_context` for raw GL access — this is
//! deliberately GLES-specific (see the plan notes on why not Vulkan: no
//! full `VulkanRenderer` exists in Smithay to build against). Targets
//! GLES 3.x core features (confirmed present in this environment: logged
//! "OpenGL ES 3.2 Mesa" at startup) — single-channel `RED` textures and
//! core instancing, not the GLES2 + extensions fallback path.
//!
//! [`GlyphAtlas`] (the shader program + atlas texture + rasterization
//! cache) is shared between [`GpuTermRenderer`] (one ConsoleHut's full terminal
//! grid) and [`LabelRenderer`] (Phase 4's tab-strip chrome — short
//! standalone strings like window titles) via `Rc<RefCell<_>>`, so a
//! glyph seen by one is cached for the other too rather than rasterized
//! and uploaded twice into two separate atlases.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture, ffi};
use smithay::utils::{Buffer, Point, Rectangle, Size};

use mudhuts_term::GlyphCache;
use mudhuts_term::palette::Rgb;
use mudhuts_term::render::{CellInfo, Damage, damage_bounds};

// 2048, not 1024: a `ShelfPacker` with no eviction (see its own doc
// comment) means a single very long-lived terminal cycling through many
// distinct glyphs (rotating logs, heavy CJK/emoji output) can exhaust a
// small atlas, at which point new glyphs silently stop rendering for the
// rest of that terminal's session. Quadrupling capacity for a few extra
// MB of per-Hut GPU memory (see `queue_gl_delete`'s doc comment) makes
// that far less likely to matter in practice, without taking on real
// eviction's own hazard: evicting a glyph still visible on screen would
// cause visible corruption, which is worse than a new glyph not
// appearing.
const ATLAS_SIZE: u32 = 2048;

/// A raw GL object (program/buffer/framebuffer) queued for deletion once a
/// renderer is next available — see [`queue_gl_delete`]'s doc comment for
/// why this indirection exists instead of deleting directly from `Drop`.
enum PendingGlDelete {
    Program(ffi::types::GLuint),
    Buffer(ffi::types::GLuint),
    Framebuffer(ffi::types::GLuint),
    Texture(ffi::types::GLuint),
}

thread_local! {
    static PENDING_GL_DELETES: RefCell<Vec<PendingGlDelete>> = const { RefCell::new(Vec::new()) };
}

/// Queue a raw GL object for deletion — called from `Drop` impls below,
/// which have no way to reach a `&mut GlesRenderer`/`&Gles2` (a `Drop::drop`
/// takes no extra arguments, and a ConsoleHut can be torn down from
/// contexts with no renderer in scope at all, e.g. `State::handle_term_event`
/// reacting to a shell exit, or an Alt-Tab discard in `input.rs` — neither
/// runs anywhere near a render pass). Mirrors the pattern Smithay's own
/// `GlesTexture` uses for exactly the same reason (see
/// `GlesTextureInternal`'s `Drop` impl in
/// `.../backend/renderer/gles/texture.rs`: it sends the id through a
/// `destruction_callback_sender` channel rather than deleting inline) —
/// the only difference is Smithay's queue is internal to `GlesRenderer`
/// and drained automatically on every bind, while this one is drained
/// explicitly by [`drain_pending_gl_deletes`], called once at the top of
/// `render::build_frame_elements` (the one place both backends already
/// hand a live `&mut GlesRenderer` through every frame).
///
/// Before this queue existed, every `GlyphAtlas`/`GpuTermRenderer`/
/// `LabelRenderer` — one full set per ConsoleHut, recreated for every new
/// terminal — leaked its shader program, VBO(s), framebuffer, and (for
/// `GlyphAtlas` specifically) its full `ATLAS_SIZE`×`ATLAS_SIZE` texture
/// (2048×2048 single-channel = exactly 4MB) for the rest of the
/// compositor's lifetime: opening and closing a terminal repeatedly grew
/// driver-side GL/GPU memory without bound, never reclaimed until mudhuts
/// itself exited.
fn queue_gl_delete(item: PendingGlDelete) {
    PENDING_GL_DELETES.with(|queue| queue.borrow_mut().push(item));
}

/// Actually delete whatever [`queue_gl_delete`] has queued up since the
/// last call — see that function's doc comment. Safe/cheap to call every
/// frame even when nothing's queued (the common case): an empty `Vec`
/// check with no `with_context` call at all.
pub fn drain_pending_gl_deletes(renderer: &mut GlesRenderer) {
    let pending = PENDING_GL_DELETES.with(|queue| std::mem::take(&mut *queue.borrow_mut()));
    if pending.is_empty() {
        return;
    }
    let result = renderer.with_context(|gl| unsafe {
        for item in pending {
            match item {
                PendingGlDelete::Program(id) => gl.DeleteProgram(id),
                PendingGlDelete::Buffer(id) => gl.DeleteBuffers(1, &id),
                PendingGlDelete::Framebuffer(id) => gl.DeleteFramebuffers(1, &id),
                PendingGlDelete::Texture(id) => gl.DeleteTextures(1, &id),
            }
        }
    });
    if let Err(err) = result {
        tracing::warn!("failed to drain pending GL object deletions: {err}");
    }
}

/// The glyph atlas is looked up once per on-screen cell every redraw, so its
/// hash cost is on the hot path. `HashMap`'s default hasher (SipHash) is
/// built for DoS resistance against attacker-controlled keys, which this
/// tiny internal `(char, bool)` cache doesn't need — profiling a live
/// instance under `perf` showed SipHash mixing alone (`Sip13Rounds`,
/// `rotate_left`, `u8to64_le`) accounting for a large chunk of total CPU
/// time. This is the well-known FxHash multiply-mix (from Firefox/rustc),
/// reimplemented here rather than pulling in a dependency for ~10 lines.
#[derive(Default)]
struct FxHasher(u64);

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl Hasher for FxHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_ne_bytes(word));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.write_u64(i as u64);
    }

    fn write_u32(&mut self, i: u32) {
        self.write_u64(i as u64);
    }

    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(FX_SEED);
    }
}

type FxBuildHasher = BuildHasherDefault<FxHasher>;

const VERTEX_SHADER: &str = r#"#version 300 es
layout(location=0) in vec2 a_corner;
layout(location=1) in vec2 a_dst_pos;
layout(location=2) in vec2 a_dst_size;
layout(location=3) in vec2 a_uv_pos;
layout(location=4) in vec2 a_uv_size;
layout(location=5) in vec3 a_color;

uniform vec2 u_target_size;

out vec2 v_uv;
out vec3 v_color;

void main() {
    // No Y-flip here: `GlesTexture::from_raw` hardcodes `y_inverted:
    // false`, meaning Smithay treats texel row 0 as the visual top when
    // compositing. GL's rasterizer maps clip-space y=-1 to memory row 0
    // (bottom-left-origin convention), so row 0 of our own pixel-space
    // (a_dst_pos.y=0, our logical top) has to land at clip y=-1 — the
    // same direct mapping as x, not inverted. Inverting this flips the
    // whole image upside down (confirmed by testing — this used to be
    // `1.0 - ...`).
    vec2 px = a_dst_pos + a_corner * a_dst_size;
    vec2 clip = vec2(
        px.x / u_target_size.x * 2.0 - 1.0,
        px.y / u_target_size.y * 2.0 - 1.0
    );
    gl_Position = vec4(clip, 0.0, 1.0);
    v_uv = a_uv_pos + a_corner * a_uv_size;
    v_color = a_color;
}
"#;

const FRAGMENT_SHADER: &str = r#"#version 300 es
precision mediump float;

in vec2 v_uv;
in vec3 v_color;

uniform sampler2D u_atlas;

out vec4 o_color;

void main() {
    float coverage = texture(u_atlas, v_uv).r;
    o_color = vec4(v_color, coverage);
}
"#;

/// A quad's placement (in target pixels) and appearance (atlas UV rect +
/// color), matching the vertex shader's per-instance attributes exactly.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Instance {
    dst_pos: [f32; 2],
    dst_size: [f32; 2],
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    color: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
struct AtlasEntry {
    uv_pos: [f32; 2],
    uv_size: [f32; 2],
    xmin: i32,
    ymin: i32,
    width: u32,
    height: u32,
}

/// Simple shelf packer for the atlas — glyphs are placed left-to-right,
/// wrapping to a new shelf when a row is full. No eviction: once the
/// atlas fills up, new never-before-seen glyphs stop getting cached
/// (logged, not fatal) rather than something already on screen
/// disappearing.
struct ShelfPacker {
    cursor_x: u32,
    cursor_y: u32,
    shelf_height: u32,
}

impl ShelfPacker {
    fn new() -> Self {
        Self {
            cursor_x: 0,
            cursor_y: 0,
            shelf_height: 0,
        }
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w > ATLAS_SIZE || h > ATLAS_SIZE {
            return None;
        }
        if self.cursor_x + w > ATLAS_SIZE {
            self.cursor_x = 0;
            self.cursor_y += self.shelf_height;
            self.shelf_height = 0;
        }
        if self.cursor_y + h > ATLAS_SIZE {
            return None;
        }
        let pos = (self.cursor_x, self.cursor_y);
        self.cursor_x += w;
        self.shelf_height = self.shelf_height.max(h);
        Some(pos)
    }
}

fn compile_shader(
    gl: &ffi::Gles2,
    kind: ffi::types::GLenum,
    source: &str,
) -> Result<ffi::types::GLuint, String> {
    let src = std::ffi::CString::new(source).map_err(|e| e.to_string())?;
    unsafe {
        let shader = gl.CreateShader(kind);
        gl.ShaderSource(shader, 1, &src.as_ptr(), std::ptr::null());
        gl.CompileShader(shader);

        let mut status = ffi::FALSE as ffi::types::GLint;
        gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut status);
        if status == ffi::TRUE as ffi::types::GLint {
            return Ok(shader);
        }

        let mut len = 0;
        gl.GetShaderiv(shader, ffi::INFO_LOG_LENGTH, &mut len);
        let mut buf = vec![0u8; len.max(1) as usize];
        let mut written = 0;
        gl.GetShaderInfoLog(
            shader,
            len,
            &mut written,
            buf.as_mut_ptr() as *mut ffi::types::GLchar,
        );
        buf.truncate(written.max(0) as usize);
        gl.DeleteShader(shader);
        Err(String::from_utf8_lossy(&buf).into_owned())
    }
}

fn link_program(
    gl: &ffi::Gles2,
    vertex: &str,
    fragment: &str,
) -> Result<ffi::types::GLuint, String> {
    unsafe {
        let vs = compile_shader(gl, ffi::VERTEX_SHADER, vertex)
            .map_err(|e| format!("vertex shader: {e}"))?;
        let fs = compile_shader(gl, ffi::FRAGMENT_SHADER, fragment)
            .map_err(|e| format!("fragment shader: {e}"))?;

        let program = gl.CreateProgram();
        gl.AttachShader(program, vs);
        gl.AttachShader(program, fs);
        gl.LinkProgram(program);
        gl.DeleteShader(vs);
        gl.DeleteShader(fs);

        let mut status = ffi::FALSE as ffi::types::GLint;
        gl.GetProgramiv(program, ffi::LINK_STATUS, &mut status);
        if status == ffi::TRUE as ffi::types::GLint {
            return Ok(program);
        }

        let mut len = 0;
        gl.GetProgramiv(program, ffi::INFO_LOG_LENGTH, &mut len);
        let mut buf = vec![0u8; len.max(1) as usize];
        let mut written = 0;
        gl.GetProgramInfoLog(
            program,
            len,
            &mut written,
            buf.as_mut_ptr() as *mut ffi::types::GLchar,
        );
        buf.truncate(written.max(0) as usize);
        gl.DeleteProgram(program);
        Err(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// The shader program, glyph-coverage atlas texture, and rasterization
/// cache shared by every renderer that draws text (currently
/// [`GpuTermRenderer`] and [`LabelRenderer`]) — see the module doc for why
/// this is split out and shared rather than duplicated per renderer.
pub struct GlyphAtlas {
    program: ffi::types::GLuint,
    u_target_size: ffi::types::GLint,
    quad_vbo: ffi::types::GLuint,
    atlas_tex: ffi::types::GLuint,
    packer: ShelfPacker,
    glyphs: HashMap<(char, bool), AtlasEntry, FxBuildHasher>,
    white: AtlasEntry,
}

impl Drop for GlyphAtlas {
    fn drop(&mut self) {
        // See `queue_gl_delete`'s doc comment — this is the one that used
        // to leak a full 4MB `ATLAS_SIZE`×`ATLAS_SIZE` texture per
        // ConsoleHut.
        queue_gl_delete(PendingGlDelete::Program(self.program));
        queue_gl_delete(PendingGlDelete::Buffer(self.quad_vbo));
        queue_gl_delete(PendingGlDelete::Texture(self.atlas_tex));
    }
}

impl GlyphAtlas {
    pub fn new(renderer: &mut GlesRenderer) -> Result<Self, String> {
        renderer
            .with_context(|gl| unsafe {
                let program = link_program(gl, VERTEX_SHADER, FRAGMENT_SHADER)?;
                // A C-string literal (not `CString::new(...).unwrap()`) —
                // infallible by construction, no embedded NUL to fail on.
                let u_target_size = gl.GetUniformLocation(program, c"u_target_size".as_ptr());

                // A single unit quad (triangle strip), reused for every
                // instance via per-instance attributes.
                let mut quad_vbo = 0;
                gl.GenBuffers(1, &mut quad_vbo);
                gl.BindBuffer(ffi::ARRAY_BUFFER, quad_vbo);
                let corners: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
                gl.BufferData(
                    ffi::ARRAY_BUFFER,
                    (corners.len() * 4) as ffi::types::GLsizeiptr,
                    corners.as_ptr() as *const _,
                    ffi::STATIC_DRAW,
                );

                let mut atlas_tex = 0;
                gl.GenTextures(1, &mut atlas_tex);
                gl.BindTexture(ffi::TEXTURE_2D, atlas_tex);
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_MIN_FILTER,
                    ffi::NEAREST as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_MAG_FILTER,
                    ffi::NEAREST as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.PixelStorei(ffi::UNPACK_ALIGNMENT, 1);
                let blank = vec![0u8; (ATLAS_SIZE * ATLAS_SIZE) as usize];
                gl.TexImage2D(
                    ffi::TEXTURE_2D,
                    0,
                    ffi::RED as i32,
                    ATLAS_SIZE as i32,
                    ATLAS_SIZE as i32,
                    0,
                    ffi::RED,
                    ffi::UNSIGNED_BYTE,
                    blank.as_ptr() as *const _,
                );

                let mut packer = ShelfPacker::new();
                let (wx, wy) = packer
                    .alloc(2, 2)
                    .ok_or("atlas too small for the reserved solid pixel")?;
                let white_px = [255u8; 4];
                gl.TexSubImage2D(
                    ffi::TEXTURE_2D,
                    0,
                    wx as i32,
                    wy as i32,
                    2,
                    2,
                    ffi::RED,
                    ffi::UNSIGNED_BYTE,
                    white_px.as_ptr() as *const _,
                );
                let white = AtlasEntry {
                    uv_pos: [wx as f32 / ATLAS_SIZE as f32, wy as f32 / ATLAS_SIZE as f32],
                    uv_size: [1.0 / ATLAS_SIZE as f32, 1.0 / ATLAS_SIZE as f32],
                    xmin: 0,
                    ymin: 0,
                    width: 1,
                    height: 1,
                };

                Ok(GlyphAtlas {
                    program,
                    u_target_size,
                    quad_vbo,
                    atlas_tex,
                    packer,
                    glyphs: HashMap::default(),
                    white,
                })
            })
            .map_err(|e| e.to_string())?
    }

    /// Look up (or rasterize + upload) the atlas entry for `(c, bold)`.
    fn atlas_entry(
        &mut self,
        gl: &ffi::Gles2,
        glyph_cache: &mut GlyphCache,
        c: char,
        bold: bool,
    ) -> Option<AtlasEntry> {
        if let Some(entry) = self.glyphs.get(&(c, bold)) {
            return Some(*entry);
        }

        let (metrics, bitmap) = glyph_cache.glyph(c, bold);
        let (width, height) = (metrics.width as u32, metrics.height as u32);
        if width == 0 || height == 0 {
            // Whitespace-shaped glyphs (e.g. space itself, though callers
            // skip those already) — nothing to draw, nothing to cache.
            return None;
        }

        let Some((x, y)) = self.packer.alloc(width, height) else {
            tracing::warn!("glyph atlas is full; {c:?} (bold={bold}) won't render until it grows");
            return None;
        };

        unsafe {
            gl.BindTexture(ffi::TEXTURE_2D, self.atlas_tex);
            gl.TexSubImage2D(
                ffi::TEXTURE_2D,
                0,
                x as i32,
                y as i32,
                width as i32,
                height as i32,
                ffi::RED,
                ffi::UNSIGNED_BYTE,
                bitmap.as_ptr() as *const _,
            );
        }

        let entry = AtlasEntry {
            uv_pos: [x as f32 / ATLAS_SIZE as f32, y as f32 / ATLAS_SIZE as f32],
            uv_size: [
                width as f32 / ATLAS_SIZE as f32,
                height as f32 / ATLAS_SIZE as f32,
            ],
            xmin: metrics.xmin,
            ymin: metrics.ymin,
            width,
            height,
        };
        self.glyphs.insert((c, bold), entry);
        Some(entry)
    }
}

/// Shared draw sequence: upload `instances` and draw them as two
/// instanced batches (backgrounds — `instances[..bg_count]` — then
/// glyphs) into `fbo`'s `tex_size`-sized target, sampling `atlas_tex`
/// through `program`. Used by both [`GpuTermRenderer::redraw`] (a whole
/// terminal grid) and [`LabelRenderer::render`] (one short string) — same
/// shader/vertex layout, different content and target.
///
/// `scissor` — `(x, y, width, height)`, in the *same* raw window/pixel
/// coordinate space `dst_pos`/`u_target_size` already use (see
/// `GpuTermRenderer::redraw`'s doc comment on why no separate Y-flip
/// conversion is needed to compute it) — clips both the clear and the
/// instanced draws below to that region, `None` for the normal
/// whole-target clear+redraw. Always disabled again before returning
/// (regardless of whether it was enabled), since `gl` is a shared context
/// other draw calls reuse afterward.
#[allow(clippy::too_many_arguments)]
fn draw_instances(
    gl: &ffi::Gles2,
    program: ffi::types::GLuint,
    u_target_size: ffi::types::GLint,
    quad_vbo: ffi::types::GLuint,
    atlas_tex: ffi::types::GLuint,
    instance_vbo: ffi::types::GLuint,
    instance_capacity: &mut usize,
    fbo: ffi::types::GLuint,
    tex_size: (i32, i32),
    instances: &[Instance],
    bg_count: usize,
    scissor: Option<(i32, i32, i32, i32)>,
) {
    unsafe {
        gl.BindBuffer(ffi::ARRAY_BUFFER, instance_vbo);
        let needed = instances.len().max(1);
        let byte_len = (needed * std::mem::size_of::<Instance>()) as ffi::types::GLsizeiptr;
        if needed > *instance_capacity {
            gl.BufferData(
                ffi::ARRAY_BUFFER,
                byte_len,
                std::ptr::null(),
                ffi::DYNAMIC_DRAW,
            );
            *instance_capacity = needed;
        }
        if !instances.is_empty() {
            gl.BufferSubData(
                ffi::ARRAY_BUFFER,
                0,
                std::mem::size_of_val(instances) as ffi::types::GLsizeiptr,
                instances.as_ptr() as *const _,
            );
        }

        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.Viewport(0, 0, tex_size.0, tex_size.1);
        if let Some((x, y, w, h)) = scissor {
            gl.Enable(ffi::SCISSOR_TEST);
            gl.Scissor(x, y, w, h);
        }
        gl.ClearColor(0.0, 0.0, 0.0, 1.0);
        gl.Clear(ffi::COLOR_BUFFER_BIT);

        gl.Enable(ffi::BLEND);
        gl.BlendFunc(ffi::SRC_ALPHA, ffi::ONE_MINUS_SRC_ALPHA);

        gl.UseProgram(program);
        gl.Uniform2f(u_target_size, tex_size.0 as f32, tex_size.1 as f32);
        gl.ActiveTexture(ffi::TEXTURE0);
        gl.BindTexture(ffi::TEXTURE_2D, atlas_tex);

        gl.BindBuffer(ffi::ARRAY_BUFFER, quad_vbo);
        gl.EnableVertexAttribArray(0);
        gl.VertexAttribPointer(0, 2, ffi::FLOAT, ffi::FALSE, 0, std::ptr::null());

        let stride = std::mem::size_of::<Instance>() as ffi::types::GLsizei;
        let attrib = |index: u32, size: i32, offset: usize, divisor: u32| {
            gl.BindBuffer(ffi::ARRAY_BUFFER, instance_vbo);
            gl.EnableVertexAttribArray(index);
            gl.VertexAttribPointer(
                index,
                size,
                ffi::FLOAT,
                ffi::FALSE,
                stride,
                offset as *const _,
            );
            gl.VertexAttribDivisor(index, divisor);
        };

        let draw = |base: usize, count: usize| {
            if count == 0 {
                return;
            }
            let base_bytes = base * std::mem::size_of::<Instance>();
            attrib(1, 2, base_bytes, 1);
            attrib(2, 2, base_bytes + 8, 1);
            attrib(3, 2, base_bytes + 16, 1);
            attrib(4, 2, base_bytes + 24, 1);
            attrib(5, 3, base_bytes + 32, 1);
            gl.DrawArraysInstanced(ffi::TRIANGLE_STRIP, 0, 4, count as ffi::types::GLsizei);
        };
        draw(0, bg_count);
        draw(bg_count, instances.len() - bg_count);

        // Unconditionally, even if `scissor` was `None` (a no-op then,
        // `SCISSOR_TEST` starts disabled and nothing else in this shared
        // context enables it) — see this function's own doc comment.
        gl.Disable(ffi::SCISSOR_TEST);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
    }
}

/// Allocate a fresh `width`x`height` RGBA8 texture bound as `fbo`'s color
/// attachment — always a *new* GL texture id (never reusing a previous
/// one), so a still-in-flight `GlesTexture` wrapper from a previous call
/// safely owns cleanup of its old id on its own schedule instead of
/// racing with this one. Shared by `GpuTermRenderer::ensure_size` and
/// `LabelRenderer::render`.
fn alloc_color_target(
    renderer: &mut GlesRenderer,
    fbo: ffi::types::GLuint,
    width: i32,
    height: i32,
) -> Result<GlesTexture, String> {
    let color_tex = renderer
        .with_context(|gl| unsafe {
            let mut color_tex = 0;
            gl.GenTextures(1, &mut color_tex);
            gl.BindTexture(ffi::TEXTURE_2D, color_tex);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA as i32,
                width,
                height,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null(),
            );

            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                color_tex,
                0,
            );
            let status = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, 0);
            if status != ffi::FRAMEBUFFER_COMPLETE {
                return Err(format!("incomplete framebuffer: 0x{status:x}"));
            }
            Ok(color_tex)
        })
        .map_err(|e| e.to_string())??;

    // Safety: `color_tex` was just created by us above, is a valid 2D
    // RGBA8 texture of exactly `width`x`height`, and we're handing
    // ownership to this wrapper (never calling `glDeleteTextures` on it
    // ourselves).
    Ok(unsafe {
        GlesTexture::from_raw(
            renderer,
            Some(ffi::RGBA),
            true,
            color_tex,
            (width, height).into(),
        )
    })
}

pub struct GpuTermRenderer {
    atlas: Rc<RefCell<GlyphAtlas>>,
    instance_vbo: ffi::types::GLuint,
    instance_capacity: usize,
    fbo: ffi::types::GLuint,
    /// The current offscreen color target, wrapped once per (re)allocation
    /// — *not* re-wrapped every frame. `GlesTexture::from_raw` takes
    /// ownership semantics (it's `Arc`-backed and frees the underlying GL
    /// texture once the last clone drops); re-wrapping the same GL texture
    /// id fresh each frame would let one frame's wrapper free the texture
    /// out from under the next frame's. Cloning this (cheap: an `Arc`
    /// clone) is the correct way to hand it to a render element each frame.
    color_texture: Option<GlesTexture>,
    tex_size: (i32, i32),
}

impl Drop for GpuTermRenderer {
    fn drop(&mut self) {
        // `color_texture` self-cleans (a `GlesTexture` frees its own GL
        // texture via Smithay's own destruction-callback queue when
        // dropped) — only the two raw ids need queuing here. See
        // `queue_gl_delete`'s doc comment.
        queue_gl_delete(PendingGlDelete::Buffer(self.instance_vbo));
        queue_gl_delete(PendingGlDelete::Framebuffer(self.fbo));
    }
}

impl GpuTermRenderer {
    /// Creates its own atlas. Most callers want [`Self::with_atlas`]
    /// instead, sharing one with the same ConsoleHut's [`LabelRenderer`] (if any)
    /// so glyphs aren't rasterized/uploaded twice.
    pub fn new(renderer: &mut GlesRenderer) -> Result<Self, String> {
        let atlas = Rc::new(RefCell::new(GlyphAtlas::new(renderer)?));
        Self::with_atlas(renderer, atlas)
    }

    pub fn with_atlas(
        renderer: &mut GlesRenderer,
        atlas: Rc<RefCell<GlyphAtlas>>,
    ) -> Result<Self, String> {
        renderer
            .with_context(|gl| unsafe {
                let mut instance_vbo = 0;
                gl.GenBuffers(1, &mut instance_vbo);
                let mut fbo = 0;
                gl.GenFramebuffers(1, &mut fbo);
                GpuTermRenderer {
                    atlas,
                    instance_vbo,
                    instance_capacity: 0,
                    fbo,
                    color_texture: None,
                    tex_size: (0, 0),
                }
            })
            .map_err(|e| e.to_string())
    }

    /// This renderer's glyph atlas, to share with a [`LabelRenderer`] for
    /// the same ConsoleHut.
    pub fn atlas(&self) -> Rc<RefCell<GlyphAtlas>> {
        self.atlas.clone()
    }

    /// (Re)create the offscreen color target if `width`x`height` changed.
    /// Returns `true` if it was (re)created, meaning the whole thing needs
    /// redrawing this frame.
    fn ensure_size(
        &mut self,
        renderer: &mut GlesRenderer,
        width: i32,
        height: i32,
    ) -> Result<bool, String> {
        if (width, height) == self.tex_size || width <= 0 || height <= 0 {
            return Ok(false);
        }
        self.color_texture = Some(alloc_color_target(renderer, self.fbo, width, height)?);
        self.tex_size = (width, height);
        Ok(true)
    }

    /// Redraw `cells` into the offscreen target (resized to `width`x
    /// `height` if needed), returning the up-to-date `GlesTexture`. Only
    /// actually clears/redraws `damage`'s own bounding box (see
    /// `damage_bounds`'s doc comment) rather than the whole target —
    /// correct because `color_texture` is one persistently-reused GPU
    /// texture, not reallocated fresh each call (see its own field doc):
    /// whatever this call *doesn't* touch is still exactly what the
    /// previous call left there, real prior content, not garbage. `cells`
    /// itself must already be limited to that same bounding box (see
    /// `mudhuts_term::render::collect_cells`) — the scissor rect below is
    /// computed independently from `damage`, not derived from `cells`,
    /// so the two have to already agree or real content would get
    /// clipped out.
    ///
    /// The scissor rect needs no Y-flip/coordinate conversion despite
    /// `VERTEX_SHADER`'s clip-space math looking like it flips things:
    /// `clip.y = px.y/target_size.y*2-1` combined with the standard GL
    /// NDC-to-window mapping `winY = (clip.y+1)/2*H` algebraically
    /// simplifies to `winY = px.y` exactly (`H` cancels) — so `glScissor`
    /// takes the *same* raw `row * cell_h` value `dst_pos.y` already
    /// uses, not `tex_size.1` minus it. This holds regardless of whether
    /// something downstream additionally flips the finished texture for
    /// display — that's a separate concern from whether the scissor rect
    /// lines up with where *this shader* itself places quads, which it
    /// does by construction (same formula, same inputs).
    #[allow(clippy::too_many_arguments)]
    pub fn redraw(
        &mut self,
        renderer: &mut GlesRenderer,
        glyph_cache: &mut GlyphCache,
        cells: &[CellInfo],
        damage: &Damage,
        cell_w: usize,
        cell_h: usize,
        baseline: usize,
        width: i32,
        height: i32,
    ) -> Result<(GlesTexture, Option<Rectangle<i32, Buffer>>), String> {
        let resized = self.ensure_size(renderer, width, height)?;

        let white = self.atlas.borrow().white;
        let mut instances = Vec::with_capacity(cells.len());
        for cell in cells {
            instances.push(Instance {
                dst_pos: [(cell.col * cell_w) as f32, (cell.row * cell_h) as f32],
                dst_size: [cell_w as f32, cell_h as f32],
                uv_pos: white.uv_pos,
                uv_size: white.uv_size,
                color: rgb_f32(cell.bg),
            });
        }
        let bg_count = instances.len();
        let tex_size = self.tex_size;

        // Resolve glyph atlas entries (may upload new glyphs) directly
        // into placed instances — no placeholder pass first: an earlier
        // version pushed a blank `Instance` per glyph cell up front, only
        // to `truncate`/discard all of them once this real pass built the
        // actual list, wasted allocation and writes on every redraw. One
        // `with_context` for the whole batch, not one per cell either —
        // it does a real `eglMakeCurrent` every call with no already-
        // current short-circuit, so calling it per-glyph turned a redraw
        // of e.g. ~200 visible glyphs into ~200 driver calls instead of 1.
        let atlas = &self.atlas;
        let glyph_instances: Vec<Instance> = renderer
            .with_context(|gl| {
                let mut atlas = atlas.borrow_mut();
                let mut out = Vec::with_capacity(cells.len());
                for cell in cells {
                    if cell.c == ' ' || cell.c == '\0' {
                        continue;
                    }
                    let Some(entry) = atlas.atlas_entry(gl, glyph_cache, cell.c, cell.bold) else {
                        continue;
                    };
                    let glyph_x = (cell.col * cell_w) as i32 + entry.xmin;
                    let glyph_y =
                        (cell.row * cell_h) as i32 + baseline as i32 - entry.height as i32 - entry.ymin;
                    out.push(Instance {
                        dst_pos: [glyph_x as f32, glyph_y as f32],
                        dst_size: [entry.width as f32, entry.height as f32],
                        uv_pos: entry.uv_pos,
                        uv_size: entry.uv_size,
                        color: rgb_f32(cell.fg),
                    });
                }
                out
            })
            .map_err(|e| e.to_string())?;
        instances.reserve(glyph_instances.len());
        instances.extend(glyph_instances);

        // A freshly (re)allocated target has no prior content to preserve
        // at all — `ensure_size`'s own doc comment: "returns `true` if it
        // was (re)created, meaning the whole thing needs redrawing" —
        // regardless of what `damage` itself says (a resize also implies
        // a fresh `Terminal::render`/first-ever redraw upstream, but
        // don't rely on that alone; this is the one place that actually
        // knows whether the *target* is fresh).
        let touched: Option<Rectangle<i32, Buffer>> = if resized {
            None
        } else {
            damage_bounds(damage).map(|(min_line, max_line, min_col, max_col)| {
                let x = (min_col * cell_w) as i32;
                let y = (min_line * cell_h) as i32;
                let w = ((max_col - min_col + 1) * cell_w) as i32;
                let h = ((max_line - min_line + 1) * cell_h) as i32;
                Rectangle::new(Point::from((x, y)), Size::from((w, h)))
            })
        };
        let scissor = touched.map(|r| (r.loc.x, r.loc.y, r.size.w, r.size.h));

        let (instance_vbo, fbo) = (self.instance_vbo, self.fbo);
        let atlas = self.atlas.borrow();
        let (program, u_target_size, quad_vbo, atlas_tex) = (
            atlas.program,
            atlas.u_target_size,
            atlas.quad_vbo,
            atlas.atlas_tex,
        );
        drop(atlas);
        renderer
            .with_context(|gl| {
                draw_instances(
                    gl,
                    program,
                    u_target_size,
                    quad_vbo,
                    atlas_tex,
                    instance_vbo,
                    &mut self.instance_capacity,
                    fbo,
                    tex_size,
                    &instances,
                    bg_count,
                    scissor,
                )
            })
            .map_err(|e| e.to_string())?;

        // Cheap: an `Arc` clone (see the `color_texture` field doc),
        // pointing at the same GPU texture `ensure_size` just rendered
        // into above — not a pixel copy.
        let texture = self
            .color_texture
            .clone()
            .ok_or_else(|| "no color texture allocated (width/height were <= 0?)".to_string())?;
        Ok((texture, touched))
    }
}

/// Renders one short standalone string (no wrapping, single line) into its
/// own texture sized to fit it exactly — used for Phase 4's tab-strip
/// chrome (window titles), not the terminal grid. Unlike
/// [`GpuTermRenderer`], always allocates a fresh target texture per call
/// rather than reusing one when the size matches: labels are small,
/// low-frequency (only rebuilt when the tab strip's contents change), so
/// the reuse optimization that matters for the full-screen terminal path
/// isn't worth the complexity here.
pub struct LabelRenderer {
    atlas: Rc<RefCell<GlyphAtlas>>,
    instance_vbo: ffi::types::GLuint,
    instance_capacity: usize,
    fbo: ffi::types::GLuint,
}

impl Drop for LabelRenderer {
    fn drop(&mut self) {
        // Its own `instance_vbo`/`fbo`, separate from `GpuTermRenderer`'s —
        // see that impl's matching `Drop`. `atlas` is an `Rc` clone of the
        // same `GlyphAtlas` `GpuTermRenderer` holds; its own `Drop` only
        // runs once the last clone (whichever of the two structs for this
        // ConsoleHut drops last) goes away.
        queue_gl_delete(PendingGlDelete::Buffer(self.instance_vbo));
        queue_gl_delete(PendingGlDelete::Framebuffer(self.fbo));
    }
}

impl LabelRenderer {
    pub fn new(
        renderer: &mut GlesRenderer,
        atlas: Rc<RefCell<GlyphAtlas>>,
    ) -> Result<Self, String> {
        renderer
            .with_context(|gl| unsafe {
                let mut instance_vbo = 0;
                gl.GenBuffers(1, &mut instance_vbo);
                let mut fbo = 0;
                gl.GenFramebuffers(1, &mut fbo);
                LabelRenderer {
                    atlas,
                    instance_vbo,
                    instance_capacity: 0,
                    fbo,
                }
            })
            .map_err(|e| e.to_string())
    }

    /// Render `text` (single line; caller should pre-truncate to whatever
    /// fits its layout) with `fg`-colored glyphs over a `bg`-colored
    /// background, `cell_w`/`cell_h`/`baseline` matching the compositor's
    /// glyph metrics so labels visually match the terminal's own type.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        glyph_cache: &mut GlyphCache,
        text: &str,
        cell_w: usize,
        cell_h: usize,
        baseline: usize,
        fg: Rgb,
        bg: Rgb,
    ) -> Result<GlesTexture, String> {
        let chars: Vec<char> = text.chars().collect();
        let width = (chars.len().max(1) * cell_w) as i32;
        let height = cell_h.max(1) as i32;

        let texture = alloc_color_target(renderer, self.fbo, width, height)?;

        let white = self.atlas.borrow().white;
        let mut instances = vec![Instance {
            dst_pos: [0.0, 0.0],
            dst_size: [width as f32, height as f32],
            uv_pos: white.uv_pos,
            uv_size: white.uv_size,
            color: rgb_f32(bg),
        }];
        let bg_count = instances.len();

        // One `with_context` (real `eglMakeCurrent`) for the whole label,
        // not one per glyph — see `GpuTermRenderer::redraw`'s identical
        // fix/doc comment for why that matters.
        let atlas = &self.atlas;
        let glyph_instances: Vec<Instance> = renderer
            .with_context(|gl| {
                let mut atlas = atlas.borrow_mut();
                let mut out = Vec::with_capacity(chars.len());
                for (i, &c) in chars.iter().enumerate() {
                    if c == ' ' {
                        continue;
                    }
                    let Some(entry) = atlas.atlas_entry(gl, glyph_cache, c, false) else {
                        continue;
                    };
                    let glyph_x = (i * cell_w) as i32 + entry.xmin;
                    let glyph_y = baseline as i32 - entry.height as i32 - entry.ymin;
                    out.push(Instance {
                        dst_pos: [glyph_x as f32, glyph_y as f32],
                        dst_size: [entry.width as f32, entry.height as f32],
                        uv_pos: entry.uv_pos,
                        uv_size: entry.uv_size,
                        color: rgb_f32(fg),
                    });
                }
                out
            })
            .map_err(|e| e.to_string())?;
        instances.extend(glyph_instances);

        let (instance_vbo, fbo) = (self.instance_vbo, self.fbo);
        let atlas = self.atlas.borrow();
        let (program, u_target_size, quad_vbo, atlas_tex) = (
            atlas.program,
            atlas.u_target_size,
            atlas.quad_vbo,
            atlas.atlas_tex,
        );
        drop(atlas);
        renderer
            .with_context(|gl| {
                draw_instances(
                    gl,
                    program,
                    u_target_size,
                    quad_vbo,
                    atlas_tex,
                    instance_vbo,
                    &mut self.instance_capacity,
                    fbo,
                    (width, height),
                    &instances,
                    bg_count,
                    // Always a fresh target (this type's own doc comment:
                    // "always allocates a fresh target texture per call")
                    // — nothing to preserve, so no scissor.
                    None,
                )
            })
            .map_err(|e| e.to_string())?;

        Ok(texture)
    }
}

fn rgb_f32(rgb: Rgb) -> [f32; 3] {
    [
        rgb[0] as f32 / 255.0,
        rgb[1] as f32 / 255.0,
        rgb[2] as f32 / 255.0,
    ]
}
