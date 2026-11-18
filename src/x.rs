//! Utilitários X11: janelas ARGB, upload de imagem, propriedades EWMH e controle de janelas.

use tiny_skia::Pixmap;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

use crate::R;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        _NET_WM_NAME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_STATE,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_ABOVE,
        _NET_WM_DESKTOP,
        _NET_WM_STRUT,
        _NET_WM_STRUT_PARTIAL,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CLOSE_WINDOW,
        _NET_WM_ICON,
        _NET_WORKAREA,
        _NET_FRAME_EXTENTS,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_WINDOW_TYPE_DIALOG,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _RDOCK_RUNNING,
        _RDOCK_SETTINGS,
        _RDOCK_DND,
        XdndAware,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndSelection,
        XdndTypeList,
        XdndActionCopy,
        TEXT_URI_LIST: b"text/uri-list",
    }
}

pub fn argb_visual(screen: &Screen) -> Option<Visualid> {
    screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| &d.visuals)
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .map(|v| v.visual_id)
}

pub fn primary_monitor(conn: &RustConnection, screen: &Screen) -> (i32, i32, i32, i32) {
    let fallback = (0, 0, screen.width_in_pixels as i32, screen.height_in_pixels as i32);
    let _ = conn.randr_query_version(1, 5).map(|c| c.reply());
    let Ok(Ok(r)) = conn.randr_get_monitors(screen.root, true).map(|c| c.reply()) else { return fallback };
    r.monitors
        .iter()
        .find(|m| m.primary)
        .or(r.monitors.first())
        .map(|m| (m.x as i32, m.y as i32, m.width as i32, m.height as i32))
        .unwrap_or(fallback)
}

pub fn create_window(
    conn: &RustConnection,
    root: Window,
    visual: Visualid,
    colormap: Colormap,
    geom: (i32, i32, i32, i32),
    override_redirect: bool,
    events: EventMask,
) -> R<(Window, Surface)> {
    let win = conn.generate_id()?;
    let (x, y, w, h) = geom;
    conn.create_window(
        32,
        win,
        root,
        x as i16,
        y as i16,
        w as u16,
        h as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            .override_redirect(u32::from(override_redirect))
            .event_mask(events),
    )?;
    Ok((win, Surface::new(conn, win)?))
}

/// Destino de desenho de uma janela: envia regiões do pixmap RGBA pré-multiplicado
/// (tiny-skia) para a janela ARGB, que espera BGRA.
///
/// Usa memória compartilhada (MIT-SHM) quando o servidor permite: o servidor lê a
/// imagem direto da memória, sem copiá-la pelo socket. Isso importa no Launchpad,
/// que redesenha a tela inteira (~11 MB a 2560×1080) a cada quadro. Sem SHM
/// (ex.: X remoto), cai para PutImage.
pub struct Surface {
    gc: Gcontext,
    shm: Option<Shm>,
    shm_ok: bool,
    buf: Vec<u8>,
}

struct Shm {
    seg: shm::Seg,
    map: memmap2::MmapMut,
    size: (u32, u32),
    /// O servidor pode ainda estar lendo a memória do último envio.
    busy: bool,
}

impl Surface {
    pub fn new(conn: &RustConnection, win: Window) -> R<Surface> {
        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new())?;
        let shm_ok = conn
            .shm_query_version()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2));
        Ok(Surface { gc, shm: None, shm_ok, buf: Vec::new() })
    }

    /// Libera o segmento compartilhado (antes de destruir a janela).
    pub fn free(&mut self, conn: &RustConnection) {
        if let Some(s) = self.shm.take() {
            let _ = conn.shm_detach(s.seg);
        }
        let _ = conn.free_gc(self.gc);
    }

    /// Envia a região (x, y, w, h) do pixmap para a janela.
    pub fn put(&mut self, conn: &RustConnection, win: Window, pix: &Pixmap, rect: (i32, i32, i32, i32)) -> R<()> {
        let (pw, ph) = (pix.width() as i32, pix.height() as i32);
        let (x0, y0) = (rect.0.clamp(0, pw), rect.1.clamp(0, ph));
        let (x1, y1) = ((rect.0 + rect.2).clamp(0, pw), (rect.1 + rect.3).clamp(0, ph));
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        let (x0, y0, w, h) = (x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize);
        let stride = pw as usize * 4;
        let src = pix.data();
        let gc = self.gc;

        if let Some(shm) = self.shm_for(conn, pix.width(), pix.height()) {
            if shm.busy {
                // Uma ida e volta garante que o servidor já processou o envio anterior.
                conn.get_input_focus()?.reply()?;
            }
            for y in y0..y0 + h {
                let o = y * stride + x0 * 4;
                rgba_to_bgra(&src[o..o + w * 4], &mut shm.map[o..o + w * 4]);
            }
            let (x, y, w, h) = (x0 as u16, y0 as u16, w as u16, h as u16);
            let (tw, th) = (shm.size.0 as u16, shm.size.1 as u16);
            conn.shm_put_image(win, gc, tw, th, x, y, w, h, x as i16, y as i16, 32, ImageFormat::Z_PIXMAP.into(), false, shm.seg, 0)?;
            shm.busy = true;
            return Ok(());
        }

        let row_bytes = w * 4;
        let rows_per = ((conn.maximum_request_bytes().min(8 << 20) - 64) / row_bytes).max(1);
        let mut row = y0;
        while row < y0 + h {
            let n = rows_per.min(y0 + h - row);
            self.buf.resize(n * row_bytes, 0);
            for r in 0..n {
                let o = (row + r) * stride + x0 * 4;
                rgba_to_bgra(&src[o..o + row_bytes], &mut self.buf[r * row_bytes..][..row_bytes]);
            }
            conn.put_image(ImageFormat::Z_PIXMAP, win, self.gc, w as u16, n as u16, x0 as i16, row as i16, 0, 32, &self.buf)?;
            row += n;
        }
        Ok(())
    }

    /// Segmento compartilhado do tamanho do pixmap, criado na primeira vez (ou se o tamanho mudou).
    fn shm_for(&mut self, conn: &RustConnection, w: u32, h: u32) -> Option<&mut Shm> {
        if !self.shm_ok {
            return None;
        }
        if self.shm.as_ref().is_some_and(|s| s.size != (w, h)) {
            let _ = conn.shm_detach(self.shm.take().unwrap().seg);
        }
        if self.shm.is_none() {
            self.shm = Shm::new(conn, w, h);
            // Falhou uma vez (ex.: servidor em outra máquina): não tenta de novo.
            self.shm_ok = self.shm.is_some();
        }
        self.shm.as_mut()
    }
}

impl Shm {
    fn new(conn: &RustConnection, w: u32, h: u32) -> Option<Shm> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::path::Path::new("/dev/shm");
        let dir = if dir.is_dir() { dir.to_path_buf() } else { std::env::temp_dir() };
        let path = dir.join(format!("rdock-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        file.set_len(w as u64 * h as u64 * 4).ok()?;
        // SAFETY: o arquivo foi criado agora, já não tem nome no disco e só é compartilhado
        // com o servidor X, que apenas lê dele durante ShmPutImage.
        let map = unsafe { memmap2::MmapMut::map_mut(&file) }.ok()?;
        let seg = conn.generate_id().ok()?;
        conn.shm_attach_fd(seg, std::os::fd::OwnedFd::from(file), true).ok()?.check().ok()?;
        Some(Shm { seg, map, size: (w, h), busy: false })
    }
}

/// RGBA → BGRA (o canal alfa e a pré-multiplicação ficam iguais).
fn rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        let v = (v & 0xff00_ff00) | ((v & 0xff) << 16) | ((v >> 16) & 0xff);
        d.copy_from_slice(&v.to_le_bytes());
    }
}

pub fn prop32(conn: &RustConnection, win: Window, prop: Atom, ty: impl Into<Atom>) -> Vec<u32> {
    conn.get_property(false, win, prop, ty, 0, u32::MAX / 4)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default()
}

/// Escolhe o maior ícone de `_NET_WM_ICON` (até 256px).
pub fn wm_icon(conn: &RustConnection, atoms: &Atoms, win: Window) -> Option<Pixmap> {
    let data = prop32(conn, win, atoms._NET_WM_ICON, AtomEnum::CARDINAL);
    let mut i = 0;
    let mut best: Option<(u32, u32, usize)> = None;
    while i + 2 <= data.len() {
        let (w, h) = (data[i], data[i + 1]);
        let n = (w as usize) * (h as usize);
        if n == 0 || i + 2 + n > data.len() {
            break;
        }
        if w <= 256 && best.is_none_or(|(bw, _, _)| w > bw) {
            best = Some((w, h, i + 2));
        }
        i += 2 + n;
    }
    let (w, h, off) = best?;
    crate::icons::from_argb(w, h, &data[off..off + (w * h) as usize])
}

fn send_root_message(conn: &RustConnection, root: Window, win: Window, ty: Atom, data: [u32; 5]) {
    let ev = ClientMessageEvent::new(32, win, ty, data);
    let _ = conn.send_event(false, root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, ev);
}

pub fn activate(conn: &RustConnection, atoms: &Atoms, root: Window, win: Window, time: u32) {
    send_root_message(conn, root, win, atoms._NET_ACTIVE_WINDOW, [2, time, 0, 0, 0]);
}

pub fn close(conn: &RustConnection, atoms: &Atoms, root: Window, win: Window, time: u32) {
    send_root_message(conn, root, win, atoms._NET_CLOSE_WINDOW, [time, 2, 0, 0, 0]);
}

/// Tabela keycode → keysym (só grupo 1, com/sem Shift), suficiente para a busca do Launchpad.
pub struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn load(conn: &RustConnection) -> R<Keymap> {
        let s = conn.setup();
        let r = conn.get_keyboard_mapping(s.min_keycode, s.max_keycode - s.min_keycode + 1)?.reply()?;
        Ok(Keymap { min: s.min_keycode, per: r.keysyms_per_keycode as usize, syms: r.keysyms })
    }

    pub fn keysym(&self, code: u8, shift: bool) -> u32 {
        let base = code.saturating_sub(self.min) as usize * self.per;
        let get = |i: usize| if i < self.per { self.syms.get(base + i).copied().unwrap_or(0) } else { 0 };
        let (s0, s1) = (get(0), get(1));
        if !shift {
            return s0;
        }
        if s1 != 0 {
            return s1;
        }
        keysym_char(s0).map(|c| c.to_uppercase().next().unwrap_or(c) as u32).unwrap_or(s0)
    }
}

pub fn keysym_char(ks: u32) -> Option<char> {
    if (0x20..=0x7e).contains(&ks) || (0xa0..=0xff).contains(&ks) {
        char::from_u32(ks)
    } else if ks & 0xff00_0000 == 0x0100_0000 {
        char::from_u32(ks & 0x00ff_ffff)
    } else {
        None
    }
}
