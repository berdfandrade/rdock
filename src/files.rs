//! Arquivos: tipo MIME (shared-mime-info), ícone por tipo, arquivos recentes (recently-used.xbel)
//! e operações delegadas ao `gio` (abrir, abrir com, mover, copiar, lixeira).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::apps::{self, App};

struct MimeDb {
    /// Extensão (minúscula) → tipo MIME.
    ext: HashMap<String, String>,
    /// Tipo MIME → ícone genérico (ex.: text-x-generic).
    generic: HashMap<String, String>,
}

fn db() -> &'static MimeDb {
    static DB: OnceLock<MimeDb> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = MimeDb { ext: HashMap::new(), generic: HashMap::new() };
        for dir in apps::data_dirs().iter().rev() {
            let base = dir.join("mime");
            // globs2: "peso:tipo:*.ext", do mais forte ao mais fraco; o primeiro ganha.
            if let Ok(t) = std::fs::read_to_string(base.join("globs2")) {
                for line in t.lines() {
                    let mut f = line.splitn(3, ':');
                    let (Some(_), Some(mime), Some(glob)) = (f.next(), f.next(), f.next()) else { continue };
                    let glob = glob.split(':').next().unwrap_or("");
                    if let Some(e) = glob.strip_prefix("*.").filter(|e| !e.contains(['*', '?', '['])) {
                        db.ext.entry(e.to_lowercase()).or_insert_with(|| mime.to_string());
                    }
                }
            }
            if let Ok(t) = std::fs::read_to_string(base.join("generic-icons")) {
                for (m, i) in t.lines().filter_map(|l| l.split_once(':')) {
                    db.generic.insert(m.to_string(), i.to_string());
                }
            }
        }
        db
    })
}

pub fn mime_of(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return "inode/directory".into();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    // Tenta a extensão mais longa primeiro (tar.gz antes de gz).
    let mut rest = name.as_str();
    while let Some((_, e)) = rest.split_once('.') {
        if let Some(m) = db().ext.get(e) {
            return m.clone();
        }
        rest = e;
    }
    "application/octet-stream".into()
}

/// Chave de ícone com alternativas separadas por `|` (o carregador usa a primeira que existir).
pub fn icon_for(mime: &str) -> String {
    if mime == "inode/directory" {
        return "folder".into();
    }
    let specific = mime.replace('/', "-");
    let top = mime.split('/').next().unwrap_or("");
    let generic = db().generic.get(mime).cloned().unwrap_or_else(|| format!("{top}-x-generic"));
    format!("{specific}|{generic}|text-x-generic")
}

pub fn is_image(mime: &str) -> bool {
    mime == "image/png" || mime == "image/jpeg"
}

/// Abre um arquivo com o app padrão.
pub fn open(path: &Path) {
    run("gio", &["open".as_ref(), path.as_os_str()]);
}

/// Abre os arquivos com um app específico (pelo .desktop dele).
pub fn open_with(app: &App, paths: &[PathBuf]) {
    if app.path.as_os_str().is_empty() {
        return;
    }
    let mut args = vec!["launch".as_ref(), app.path.as_os_str()];
    args.extend(paths.iter().map(|p| p.as_os_str()));
    run("gio", &args);
}

/// Move (ou copia, com `copy`) os arquivos para a pasta `dest`.
pub fn transfer(paths: &[PathBuf], dest: &Path, copy: bool) {
    let mut args = vec![if copy { "copy" } else { "move" }.as_ref()];
    args.extend(paths.iter().filter(|p| p.parent() != Some(dest)).map(|p| p.as_os_str()));
    if args.len() > 1 {
        args.push(dest.as_os_str());
        run("gio", &args);
    }
}

pub fn trash(paths: &[PathBuf]) {
    let mut args = vec!["trash".as_ref()];
    args.extend(paths.iter().map(|p| p.as_os_str()));
    run("gio", &args);
}

fn run(prog: &str, args: &[&std::ffi::OsStr]) {
    use std::os::unix::process::CommandExt;
    let mut c = Command::new(prog);
    c.args(args).process_group(0).stdin(std::process::Stdio::null());
    if let Ok(mut child) = c.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// `~/Projetos/x` em vez de `/home/fulano/Projetos/x`.
pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(apps::home()) {
        Ok(r) if r.as_os_str().is_empty() => "~".into(),
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Até `n` arquivos que `app` abriu por último, segundo ~/.local/share/recently-used.xbel.
pub fn recent_for(app: &App, n: usize) -> Vec<PathBuf> {
    let Ok(xml) = std::fs::read_to_string(apps::data_dirs()[0].join("recently-used.xbel")) else { return Vec::new() };
    let prog = |exec: &str| {
        let first = exec.trim_matches(|c| c == '\'' || c == '"').split_whitespace().next().unwrap_or("").to_string();
        first.rsplit('/').next().unwrap_or("").trim_matches(|c| c == '\'' || c == '"').to_string()
    };
    let me = prog(&app.exec);
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    for chunk in xml.split("<bookmark ").skip(1) {
        let Some(href) = attr(chunk, "href") else { continue };
        let Some(path) = apps::uri_to_path(&href) else { continue };
        for ap in chunk.split("<bookmark:application ").skip(1) {
            let name = attr(ap, "name").unwrap_or_default();
            let exec = unescape(&attr(ap, "exec").unwrap_or_default());
            if name == app.name || (!me.is_empty() && prog(&exec) == me) {
                found.push((attr(ap, "modified").unwrap_or_default(), path.clone()));
                break;
            }
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: Vec<PathBuf> = Vec::new();
    for (_, p) in found {
        if out.len() >= n {
            break;
        }
        if !out.contains(&p) && p.exists() {
            out.push(p);
        }
    }
    out
}

fn attr(s: &str, key: &str) -> Option<String> {
    let start = s.find(&format!("{key}=\""))? + key.len() + 2;
    let end = s[start..].find('"')? + start;
    Some(s[start..end].to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&apos;", "'").replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
