//! Choosing where frames go: GPU + KMS page flips, or the CPU renderer on the
//! boot framebuffer.
use crate::damage::DRect;
use crate::glrender::GlRenderer;
use crate::kms::FlipWait;
use crate::presenter::{MemoryFramebuffer, Pdgop, PresentStats, Presenter};
use crate::render::{Renderer, RendererKind, RendererRequest};
use wm_core::Size;

/// The presenter handed to `Server::repaint`.
// One of these exists per session and it is never moved on a frame path,
// so the size of the frame-statistics variants costs nothing worth boxing.
#[allow(clippy::large_enum_variant)]
pub enum Output {
    /// The boot framebuffer (SFGOP mapping), written by a CPU renderer.
    Boot(Pdgop),
    /// Heap framebuffer of the panel format; nothing is displayed.
    Memory(MemoryFramebuffer),
    /// The renderer owns its output (GL on KMS, or GL off-screen).
    Gpu,
}
impl Output {
    pub fn name(&self) -> &'static str {
        match self {
            Output::Boot(_) => "sfgop",
            Output::Memory(_) => "memory",
            Output::Gpu => "gpu",
        }
    }
}
impl Presenter for Output {
    fn present(&mut self, frame: &tiny_skia::Pixmap) -> Result<(), String> {
        match self {
            Output::Boot(p) => p.present(frame),
            Output::Memory(m) => m.present(frame),
            Output::Gpu => Err("a CPU frame was presented to a GPU-owned output".into()),
        }
    }
    fn present_damage(&mut self, frame: &tiny_skia::Pixmap, damage: &[DRect]) -> Result<(), String> {
        match self {
            Output::Boot(p) => p.present_damage(frame, damage),
            Output::Memory(m) => m.present_damage(frame, damage),
            Output::Gpu => Err("a CPU frame was presented to a GPU-owned output".into()),
        }
    }
    fn take_stats(&mut self) -> Option<PresentStats> {
        match self {
            Output::Boot(p) => p.take_stats(),
            Output::Memory(m) => m.take_stats(),
            Output::Gpu => None,
        }
    }
}

pub struct Selected {
    pub renderer: Box<dyn Renderer>,
    pub output: Output,
    pub size: Size,
}

/// How flip-complete events are taken; `CHONKSTEP_DARWIN_FLIP_WAIT=poll|inline|thread`.
pub fn flip_wait_from_env() -> Result<FlipWait, String> {
    match std::env::var("CHONKSTEP_DARWIN_FLIP_WAIT").as_deref() {
        Err(_) | Ok("") | Ok("poll") => Ok(FlipWait::Poll),
        // A blocking read cannot time out, so the compositor loop cannot use it.
        Ok("thread") => Ok(FlipWait::Thread),
        Ok(other) => Err(format!("CHONKSTEP_DARWIN_FLIP_WAIT={other:?}: expected poll or thread")),
    }
}

/// Open the output for a live compositor.
///
/// `Exactly(Gl)` fails if the GPU path cannot be brought up. `Auto` tries it
/// and falls back to the CPU renderer on the boot framebuffer with one log
/// line. Nothing on screen changes here: KMS sets the mode with the first
/// frame, so a failed attempt leaves the boot framebuffer as it was.
pub fn select(request: RendererRequest) -> Result<Selected, String> {
    let gl = || -> Result<Selected, String> {
        let renderer = GlRenderer::on_kms(flip_wait_from_env()?)?;
        let (w, h) = renderer.size();
        Ok(Selected { renderer: Box::new(renderer), output: Output::Gpu, size: Size::new(w, h) })
    };
    let cpu = |kind: RendererKind| -> Result<Selected, String> {
        let presenter = Pdgop::open()?;
        let size = presenter.size();
        Ok(Selected { renderer: kind.create_cpu(), output: Output::Boot(presenter), size })
    };
    match request {
        RendererRequest::Exactly(RendererKind::Gl) => gl(),
        RendererRequest::Exactly(kind) => cpu(kind),
        RendererRequest::Auto => match gl() {
            Ok(selected) => Ok(selected),
            Err(why) => {
                eprintln!("chonkstep-darwin: GL renderer unavailable ({why}); using the cpu renderer on the boot framebuffer");
                cpu(RendererKind::Damage)
            }
        },
    }
}
