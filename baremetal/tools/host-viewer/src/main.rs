//! Companion viewer for the Newton hypervisor's `host-io-semihost`
//! backend.
//!
//! - Reads `BlitEvent`s + 2 bpp payloads from `/tmp/newton-host-io/out`
//!   on a background thread; applies each blit to a shared 320×480
//!   2 bpp backing store; signals the GUI thread to repaint.
//! - The GUI thread expands the backing to BGRA32 in a `softbuffer`
//!   window and forwards mouse events as `PenEvent` records to
//!   `/tmp/newton-host-io/in`.
//!
//! Start the hypervisor first (with the `host-io-semihost` feature) so
//! it `mkdir`s the IPC dir and creates `out`; then start this viewer.
//! Restarting the hypervisor truncates `out`; the reader detects the
//! shrink and resets its position.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const SCREEN_WIDTH: u32 = 320;
const SCREEN_HEIGHT: u32 = 480;
const SCREEN_BPP: u32 = 2;
const FB_ROW_BYTES: usize = (SCREEN_WIDTH * SCREEN_BPP / 8) as usize; // 80
const FB_LEN: usize = FB_ROW_BYTES * SCREEN_HEIGHT as usize;          // 38_400

const SCALE: u32 = 2;

const OUT_PATH: &str = "/tmp/newton-host-io/out";
const IN_PATH: &str = "/tmp/newton-host-io/in";

const BLIT_HEADER_LEN: usize = 24;
const BLIT_KIND_BLIT: u8 = 1;
const BLIT_KIND_FULL_REPAINT: u8 = 2;

const PEN_EVENT_LEN: usize = 8;
const PEN_DOWN: u8 = 1;
const PEN_MOVE: u8 = 2;
const PEN_UP: u8 = 3;

/// 4-gray palette in BGRA. 2 bpp index 0 = white, 3 = black.
const PALETTE: [u32; 4] = [
    0x00FF_FFFF,
    0x00AA_AAAA,
    0x0055_5555,
    0x0000_0000,
];

#[allow(dead_code)]
struct BlitHeader {
    kind: u8,
    src_left: u16,
    src_top: u16,
    src_right: u16,
    src_bottom: u16,
    dst_left: u16,
    dst_top: u16,
    row_bytes: u16,
    payload_len: u16,
}

impl BlitHeader {
    fn parse(buf: &[u8; BLIT_HEADER_LEN]) -> Self {
        let u16le = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        BlitHeader {
            kind: buf[0],
            src_left: u16le(4),
            src_top: u16le(6),
            src_right: u16le(8),
            src_bottom: u16le(10),
            dst_left: u16le(12),
            dst_top: u16le(14),
            row_bytes: u16le(20),
            payload_len: u16le(22),
        }
    }
}

fn main() {
    std::fs::create_dir_all("/tmp/newton-host-io").ok();

    let backing = Arc::new(Mutex::new(vec![0xFFu8; FB_LEN]));
    let dirty = Arc::new(AtomicBool::new(true));

    {
        let backing = backing.clone();
        let dirty = dirty.clone();
        thread::spawn(move || io_reader_thread(backing, dirty));
    }

    // Truncate the inbound pen-event file so the hypervisor doesn't
    // see stale events from a prior session.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(IN_PATH)
        .expect("open in for truncate");

    let event_loop = EventLoop::new().expect("EventLoop");
    event_loop.set_control_flow(ControlFlow::WaitUntil(
        Instant::now() + Duration::from_millis(16),
    ));

    let in_writer = OpenOptions::new()
        .write(true)
        .append(true)
        .open(IN_PATH)
        .expect("open in for append");

    let mut app = App {
        window: None,
        surface: None,
        backing,
        dirty,
        in_writer,
        mouse_pos: PhysicalPosition::new(0.0, 0.0),
        button_down: false,
    };
    event_loop.run_app(&mut app).expect("event loop");
}

struct App {
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    backing: Arc<Mutex<Vec<u8>>>,
    dirty: Arc<AtomicBool>,
    in_writer: File,
    mouse_pos: PhysicalPosition<f64>,
    button_down: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Newton")
            .with_inner_size(LogicalSize::new(
                SCREEN_WIDTH * SCALE,
                SCREEN_HEIGHT * SCALE,
            ))
            .with_resizable(false);
        let window: Rc<Window> = Rc::new(el.create_window(attrs).expect("create_window"));
        let context = softbuffer::Context::new(window.clone()).expect("softbuffer::Context");
        let mut surface =
            softbuffer::Surface::new(&context, window.clone()).expect("softbuffer::Surface");
        let size = window.inner_size();
        surface
            .resize(
                NonZeroU32::new(size.width.max(1)).unwrap(),
                NonZeroU32::new(size.height.max(1)).unwrap(),
            )
            .expect("surface resize");
        self.window = Some(window);
        self.surface = Some(surface);
    }

    fn new_events(&mut self, el: &ActiveEventLoop, _: winit::event::StartCause) {
        if self.dirty.swap(false, Ordering::AcqRel) {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        // Re-arm the 16 ms tick.
        el.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(16),
        ));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = position;
                if self.button_down {
                    if let Some((x, y)) = self.pos_to_panel(position) {
                        self.send_pen(PEN_MOVE, x, y, 4);
                    }
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let was_down = self.button_down;
                self.button_down = state == ElementState::Pressed;
                if let Some((x, y)) = self.pos_to_panel(self.mouse_pos) {
                    match (was_down, self.button_down) {
                        // Einstein's PenDown default pressure is 4; match
                        // it in case the kernel uses a threshold.
                        (false, true) => self.send_pen(PEN_DOWN, x, y, 4),
                        (true, false) => self.send_pen(PEN_UP, x, y, 0),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

impl App {
    fn pos_to_panel(&self, pos: PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let win = self.window.as_ref()?;
        let size = win.inner_size();
        if size.width == 0 || size.height == 0 {
            return None;
        }
        let nx = pos.x / size.width as f64;
        let ny = pos.y / size.height as f64;
        if !(0.0..=1.0).contains(&nx) || !(0.0..=1.0).contains(&ny) {
            return None;
        }
        let x = (nx * SCREEN_WIDTH as f64) as u16;
        let y = (ny * SCREEN_HEIGHT as f64) as u16;
        Some((
            x.min(SCREEN_WIDTH as u16 - 1),
            y.min(SCREEN_HEIGHT as u16 - 1),
        ))
    }

    fn send_pen(&mut self, kind: u8, x: u16, y: u16, pressure: u16) {
        let mut buf = [0u8; PEN_EVENT_LEN];
        buf[0] = kind;
        buf[2..4].copy_from_slice(&x.to_le_bytes());
        buf[4..6].copy_from_slice(&y.to_le_bytes());
        buf[6..8].copy_from_slice(&pressure.to_le_bytes());
        if let Err(e) = self.in_writer.write_all(&buf) {
            eprintln!("host-viewer: write {IN_PATH}: {e}");
        } else {
            let _ = self.in_writer.flush();
        }
    }

    fn draw(&mut self) {
        let Some(surface) = self.surface.as_mut() else { return };
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let w = size.width.max(1);
        let h = size.height.max(1);
        let mut sb = surface.buffer_mut().expect("surface buffer");
        let backing = self.backing.lock().expect("backing lock");
        for py in 0..h {
            let panel_y = (py * SCREEN_HEIGHT) / h;
            let row_off = (panel_y as usize) * FB_ROW_BYTES;
            for px in 0..w {
                let panel_x = (px * SCREEN_WIDTH) / w;
                let byte = backing[row_off + (panel_x as usize) / 4];
                let shift = 6 - 2 * (panel_x % 4) as u8;
                let idx = ((byte >> shift) & 0x3) as usize;
                sb[(py * w + px) as usize] = PALETTE[idx];
            }
        }
        drop(backing);
        sb.present().expect("present");
    }
}

fn io_reader_thread(backing: Arc<Mutex<Vec<u8>>>, dirty: Arc<AtomicBool>) {
    use std::os::unix::fs::MetadataExt;

    let mut pos: u64 = 0;
    let mut last_ino: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        // Re-stat the path each cycle (not a held fd) so we catch
        // both forms of QEMU-restart event:
        //   * truncate-in-place (same inode, len drops to 0)
        //   * unlink + create   (new inode entirely)
        let pmd = match std::fs::metadata(OUT_PATH) {
            Ok(m) => m,
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let cur_ino = pmd.ino();
        let cur_len = pmd.len();
        if cur_ino != last_ino {
            // File was replaced — reset position to 0 and forget any
            // pending stream prefix from the prior session.
            pos = 0;
            last_ino = cur_ino;
            apply_stream_reset();
            // Also clear the screen-side stale pixels next paint
            // is from kind=2 full-repaint on the new session.
        }
        if cur_len < pos {
            // Same inode, file was truncated in place.
            pos = 0;
            apply_stream_reset();
        }
        if cur_len == pos {
            thread::sleep(Duration::from_millis(8));
            continue;
        }

        // Open fresh each iteration. On macOS this is essentially free
        // and avoids a stale held fd referring to an orphaned inode.
        let mut f = match File::open(OUT_PATH) {
            Ok(f) => f,
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if f.seek(SeekFrom::Start(pos)).is_err() {
            continue;
        }
        let want = (cur_len - pos).min(buf.len() as u64) as usize;
        let n = match f.read(&mut buf[..want]) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("host-viewer: read {OUT_PATH}: {e}");
                continue;
            }
        };
        if n == 0 {
            thread::sleep(Duration::from_millis(8));
            continue;
        }
        pos += n as u64;
        apply_stream(&buf[..n], &backing, &dirty);
    }
}

static RESET_PENDING: AtomicBool = AtomicBool::new(false);

fn apply_stream_reset() {
    // Tell apply_stream to drop any half-decoded prefix from the prior
    // QEMU session before processing fresh bytes.
    RESET_PENDING.store(true, Ordering::Release);
    eprintln!("host-viewer: detected QEMU restart; resetting stream");
}

fn apply_stream(bytes: &[u8], backing: &Arc<Mutex<Vec<u8>>>, dirty: &Arc<AtomicBool>) {
    // The stream is a sequence of (24-byte header, payload_len bytes
    // payload). The hypervisor's SYS_WRITE may land mid-event because
    // header and payload go through separate writes, so we buffer
    // partial bytes between calls.
    static mut PENDING: Vec<u8> = Vec::new();
    // SAFETY: io_reader_thread is the only caller; single-threaded.
    let pending = unsafe { &mut *core::ptr::addr_of_mut!(PENDING) };
    if RESET_PENDING.swap(false, Ordering::AcqRel) {
        pending.clear();
    }
    pending.extend_from_slice(bytes);
    let mut consumed = 0;
    loop {
        let avail = pending.len() - consumed;
        if avail < BLIT_HEADER_LEN {
            break;
        }
        let header_bytes: &[u8; BLIT_HEADER_LEN] =
            (&pending[consumed..consumed + BLIT_HEADER_LEN]).try_into().unwrap();
        let h = BlitHeader::parse(header_bytes);
        let total = BLIT_HEADER_LEN + h.payload_len as usize;
        if avail < total {
            break;
        }
        let payload_start = consumed + BLIT_HEADER_LEN;
        let payload_end = consumed + total;
        // Apply with a fresh borrow of `pending` that ends before the
        // `drain` at the bottom of the function.
        apply_blit(&h, &pending[payload_start..payload_end], backing);
        dirty.store(true, Ordering::Release);
        consumed = payload_end;
    }
    if consumed > 0 {
        pending.drain(..consumed);
    }
}

fn apply_blit(h: &BlitHeader, payload: &[u8], backing: &Arc<Mutex<Vec<u8>>>) {
    let mut bk = backing.lock().expect("backing lock");
    let height = h.src_bottom.saturating_sub(h.src_top) as usize;
    let width = h.src_right.saturating_sub(h.src_left) as usize;
    let row_bytes = h.row_bytes as usize;
    if row_bytes == 0 || height == 0 || width == 0 {
        return;
    }
    match h.kind {
        BLIT_KIND_FULL_REPAINT => {
            // Whole-screen sync — pixels are in display order at (0, 0).
            let n = payload.len().min(FB_LEN);
            bk[..n].copy_from_slice(&payload[..n]);
        }
        BLIT_KIND_BLIT => {
            // Per-pixel write. We can't byte-copy because dst_left and
            // src width are not guaranteed to be multiples of 4 (one
            // byte at 2 bpp) — a byte-aligned copy in the unaligned case
            // smears the padding columns over neighbouring pixels.
            let dst_left = h.dst_left as usize;
            let dst_top = h.dst_top as usize;
            for row in 0..height {
                let src_row_off = row * row_bytes;
                if src_row_off + row_bytes > payload.len() {
                    break;
                }
                let dst_row = dst_top + row;
                if dst_row >= SCREEN_HEIGHT as usize {
                    break;
                }
                let dst_row_off = dst_row * FB_ROW_BYTES;
                for col in 0..width {
                    let dst_col = dst_left + col;
                    if dst_col >= SCREEN_WIDTH as usize {
                        break;
                    }
                    let src_byte = payload[src_row_off + col / 4];
                    let src_shift = 6 - 2 * (col % 4) as u8;
                    let val = (src_byte >> src_shift) & 0x3;
                    let dst_byte_off = dst_row_off + dst_col / 4;
                    let dst_shift = 6 - 2 * (dst_col % 4) as u8;
                    let mask = 0x3u8 << dst_shift;
                    bk[dst_byte_off] = (bk[dst_byte_off] & !mask) | (val << dst_shift);
                }
            }
        }
        _ => {}
    }
}
