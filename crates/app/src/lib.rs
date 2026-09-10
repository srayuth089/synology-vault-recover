//! macOS controller for the Synology recovery app.
//!
//! The source disk is only ever opened read-only via `mbs_reader`, which has
//! no write path at all — extraction happens entirely in-process on macOS,
//! there is no external worker or VM involved.

pub mod browse;
pub mod disks;

use mbs_protocol::flags::{self, Profile};
use serde::Serialize;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;

/// What the UI needs to render the two panes.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub disks: Vec<disks::MacDisk>,
    /// Set when discovery failed, so the UI can say why instead of showing
    /// an empty list that looks like "no disks attached".
    pub error: Option<String>,
}

#[tauri::command]
fn scan_disks() -> Overview {
    match disks::list_disks() {
        Ok(disks) => Overview { disks, error: None },
        Err(e) => Overview { disks: Vec::new(), error: Some(e.to_string()) },
    }
}

/// Explain a Synology root flag value for the UI (SPEC §6).
#[tauri::command]
fn explain_root_flags(flags_hex: String) -> String {
    let raw = flags_hex.trim_start_matches("0x");
    let Ok(value) = u64::from_str_radix(raw, 16) else {
        return format!("unparseable flag value {flags_hex:?}");
    };
    match flags::check_root_flags(value, Profile::Production) {
        flags::FlagDecision::Accept => format!("0x{value:x} accepted"),
        flags::FlagDecision::Reject { unknown_bits, evidence } => format!(
            "{} (unknown bits 0x{unknown_bits:x}, evidence: {})",
            flags::describe_rejection(value, Profile::Production),
            match evidence {
                Some(flags::Evidence::OwnFixture) => "own fixture",
                Some(flags::Evidence::ExternalReport) => "external report",
                Some(flags::Evidence::Candidate) => "candidate",
                None => "none",
            }
        ),
    }
}

/// Whether the chosen destination may receive files from the chosen source.
#[tauri::command]
fn check_destination(source_bsd: String, dest_bsd: String) -> Result<(), String> {
    let all = disks::list_disks().map_err(|e| e.to_string())?;
    let find = |n: &str| all.iter().find(|d| d.bsd_name == n).cloned();
    let (Some(src), Some(dst)) = (find(&source_bsd), find(&dest_bsd)) else {
        return Err("source or destination disk is no longer attached".into());
    };
    if disks::destination_is_safe(&src, &dst) {
        Ok(())
    } else {
        Err(format!(
            "{} cannot be a destination for {}: it is the source disk or another Synology disk",
            dst.bsd_name, src.bsd_name
        ))
    }
}

/// The opened source disk, kept between browse calls.
///
/// Opening walks three metadata layers, so it is done once and reused; the
/// mutex also serialises reads against a single device handle.
type OpenState = std::sync::Mutex<Option<browse::OpenDisk>>;

#[derive(Debug, Clone, Serialize)]
pub struct OpenResult {
    pub label: String,
    pub fsid: String,
    pub bytes_used: u64,
    pub subvolumes: Vec<browse::SubvolInfo>,
}

/// Open a Synology source disk and list its subvolumes.
///
/// Raw devices are root-owned, so this reports the permission case explicitly
/// rather than as a generic failure — it is the one error a user can fix.
#[tauri::command]
fn open_source(
    device: String,
    state: tauri::State<'_, OpenState>,
) -> Result<OpenResult, String> {
    let disk = browse::open(&device).map_err(|e| match &e {
        // The user dismissed the system password dialog; that is a choice,
        // not a fault, so say so plainly instead of showing a raw error.
        browse::BrowseError::Open(_, msg) if msg.contains("declined") || msg.contains("cancel") => {
            "ยกเลิกการขอสิทธิ์ — กดที่ดิสก์อีกครั้งเพื่อลองใหม่".to_string()
        }
        _ => e.to_string(),
    })?;
    let result = OpenResult {
        label: disk.label.clone(),
        fsid: disk.fsid.clone(),
        bytes_used: disk.bytes_used,
        subvolumes: disk.subvolumes.clone(),
    };
    *state.lock().unwrap() = Some(disk);
    Ok(result)
}

/// List one directory inside an already-opened disk.
#[tauri::command]
fn list_dir(
    subvol_id: u64,
    path: String,
    state: tauri::State<'_, OpenState>,
) -> Result<Vec<browse::Entry>, String> {
    let mut guard = state.lock().unwrap();
    let disk = guard.as_mut().ok_or("ยังไม่ได้เปิดดิสก์ต้นทาง")?;
    disk.list(subvol_id, &path).map_err(|e| e.to_string())
}

/// Safely detach a disk, the way Finder's eject button does.
///
/// The open device handle is dropped first: macOS refuses to unmount a disk
/// while anything still holds it open, so closing our reader is part of
/// ejecting rather than a separate step the user has to think about.
#[tauri::command]
fn eject(
    bsd_name: String,
    open_state: tauri::State<'_, OpenState>,
    copy_state: tauri::State<'_, CopyState>,
) -> Result<String, String> {
    {
        let p = copy_state.lock().unwrap();
        if p.files_total > 0 && !p.finished {
            return Err("กำลังคัดลอกอยู่ — รอให้เสร็จก่อนถอดดิสก์".into());
        }
    }

    // Release our read handle if this is the disk we have open. Comparing on
    // the whole-disk name covers `/dev/rdisk5s5` belonging to `disk5`.
    {
        let mut guard = open_state.lock().unwrap();
        let holds_it = guard
            .as_ref()
            .map(|d| d.device.contains(bsd_name.trim_start_matches("/dev/")))
            .unwrap_or(false);
        if holds_it {
            *guard = None;
        }
    }

    let out = std::process::Command::new("/usr/sbin/diskutil")
        .args(["eject", &bsd_name])
        .output()
        .map_err(|e| format!("ถอดดิสก์ไม่สำเร็จ: {e}"))?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        // diskutil explains which process is still using the disk.
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = if msg.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { msg };
        Err(format!("ถอดดิสก์ไม่สำเร็จ: {}", msg.trim()))
    }
}

/// What the destination pane shows: a real folder, defaulting to Downloads.
#[derive(Debug, Clone, Serialize)]
pub struct LocalView {
    pub dir: String,
    pub entries: Vec<browse::LocalEntry>,
    pub free_bytes: u64,
    pub can_write: bool,
}

/// List a local folder. With no argument, opens the user's Downloads folder.
#[tauri::command]
fn list_local(dir: Option<String>) -> Result<LocalView, String> {
    let path = dir
        .map(std::path::PathBuf::from)
        .unwrap_or_else(browse::default_destination);
    // Fall back to the home folder if Downloads was removed.
    let path = if path.is_dir() {
        path
    } else {
        std::env::var_os("HOME").map(Into::into).unwrap_or(path)
    };
    let entries = browse::list_local(&path)?;
    Ok(LocalView {
        can_write: !path.metadata().map(|m| m.permissions().readonly()).unwrap_or(true),
        free_bytes: browse::free_space(&path),
        dir: path.to_string_lossy().into_owned(),
        entries,
    })
}

/// Reveal a path in Finder once a copy has finished.
#[tauri::command]
fn reveal(path: String) -> Result<(), String> {
    std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Progress of the copy currently running, if any.
type CopyState = std::sync::Mutex<browse::CopyProgress>;

/// Work out what copying `paths` into `dest` would do, without writing
/// anything — in particular, which destination files already exist.
///
/// The UI calls this before `copy_out` so it only has to ask about conflicts
/// when there actually are any.
#[tauri::command]
fn plan_copy(
    subvol_id: u64,
    paths: Vec<String>,
    dest: String,
    open_state: tauri::State<'_, OpenState>,
) -> Result<browse::CopyPlan, String> {
    let mut guard = open_state.lock().unwrap();
    let disk = guard.as_mut().ok_or("ยังไม่ได้เปิดดิสก์ต้นทาง")?;
    disk.plan_copy(subvol_id, &paths, std::path::Path::new(&dest))
        .map_err(|e| e.to_string())
}

/// Copy one or more files/folders from the source disk to a destination
/// directory, applying `policy` wherever a destination path already exists.
///
/// Runs on a worker thread: extracting a multi-gigabyte file would otherwise
/// block the UI thread for minutes. Progress is polled with `copy_progress`.
#[tauri::command]
fn copy_out(
    subvol_id: u64,
    paths: Vec<String>,
    dest: String,
    policy: browse::ConflictPolicy,
    app: tauri::AppHandle,
    open_state: tauri::State<'_, OpenState>,
    copy_state: tauri::State<'_, CopyState>,
) -> Result<(), String> {
    {
        let running = copy_state.lock().unwrap();
        if running.files_total > 0 && !running.finished {
            return Err("กำลังคัดลอกอยู่ รอให้เสร็จก่อน".into());
        }
    }
    // Take the disk out of the shared slot for the duration of the copy: the
    // reader owns one device handle and cannot be used from two threads.
    let mut disk = open_state
        .lock()
        .unwrap()
        .take()
        .ok_or("ยังไม่ได้เปิดดิสก์ต้นทาง")?;

    *copy_state.lock().unwrap() = browse::CopyProgress::default();

    std::thread::spawn(move || {
        use tauri::Manager;
        let dest_dir = std::path::PathBuf::from(dest);
        let mut report = |p: browse::CopyProgress| {
            if let Some(st) = app.try_state::<CopyState>() {
                *st.lock().unwrap() = p;
            }
        };
        let outcome = disk.copy_out(subvol_id, &paths, &dest_dir, policy, &mut report);
        if let Err(e) = outcome {
            if let Some(st) = app.try_state::<CopyState>() {
                let mut p = st.lock().unwrap();
                p.errors.push(e.to_string());
                p.finished = true;
            }
        }
        // Put the disk back so browsing can continue.
        if let Some(st) = app.try_state::<OpenState>() {
            *st.lock().unwrap() = Some(disk);
        }
    });
    Ok(())
}

#[tauri::command]
fn copy_progress(state: tauri::State<'_, CopyState>) -> browse::CopyProgress {
    state.lock().unwrap().clone()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(OpenState::default())
        .manage(CopyState::default())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            #[cfg(debug_assertions)]
            {
                use tauri::Manager;
                if let Some(w) = app.get_webview_window("main") {
                    w.open_devtools();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_disks,
            explain_root_flags,
            check_destination,
            open_source,
            list_dir,
            plan_copy,
            copy_out,
            copy_progress,
            list_local,
            reveal,
            eject
        ])
        .on_window_event(|window, event| {
            // A page-level `beforeunload` is not reliable for the native
            // window close on macOS, so the copy-in-progress guard is
            // enforced here as well: closing mid-copy would leave a
            // partially written file with no way to tell it apart from a
            // complete one.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let copying = app
                    .try_state::<CopyState>()
                    .map(|s| {
                        let p = s.lock().unwrap();
                        p.files_total > 0 && !p.finished
                    })
                    .unwrap_or(false);
                if copying {
                    api.prevent_close();
                    let window = window.clone();
                    app.dialog()
                        .message("กำลังคัดลอกไฟล์อยู่ ปิดตอนนี้จะทำให้ไฟล์ไม่ครบ\n\nต้องการปิดจริงหรือไม่?")
                        .title("กำลังคัดลอก")
                        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                            "ปิดเลย".into(),
                            "รอให้เสร็จก่อน".into(),
                        ))
                        .show(move |confirmed| {
                            if confirmed {
                                window.destroy().ok();
                            }
                        });
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
