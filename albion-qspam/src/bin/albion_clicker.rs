use evdev::{EventSummary, KeyCode};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

const FLAG_FILE: &str = "/tmp/clicker_enabled";
const BTN_FILE: &str = "/tmp/clicker_btn";

fn read_mouse_key() -> &'static str {
    match fs::read_to_string(BTN_FILE) {
        Ok(s) => match s.trim() {
            "0xC1" => "mouse:273",
            "0xC2" => "mouse:274",
            _ => "mouse:272",
        },
        Err(_) => "mouse:272",
    }
}

fn hypr_socket_path() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(xdg)
            .join("hypr")
            .join(&sig)
            .join(".socket.sock");
        if p.exists() {
            return Some(p);
        }
    }
    let fallback = PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock");
    if fallback.exists() {
        return Some(fallback);
    }
    None
}

fn send_hypr_cmd(sock: &Path, cmd: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(sock)?;
    stream.write_all(cmd.as_bytes())?;
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..n]).trim().to_string())
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let debug = args.iter().any(|a| a == "--debug" || a == "-d");
    let hold_mode = args.iter().any(|a| a == "--hold");

    let sock_path = match hypr_socket_path() {
        Some(p) => p,
        None => {
            eprintln!("找不到 Hyprland IPC socket（請確認在 Hyprland 會話中運行）");
            std::process::exit(1);
        }
    };

    if debug {
        eprintln!(
            "[DEBUG] Hyprland socket: {} | mode={}",
            sock_path.display(),
            if hold_mode { "hold" } else { "toggle" }
        );
    }

    let left_held = Arc::new(AtomicBool::new(false));
    let right_held = Arc::new(AtomicBool::new(false));
    let mut found_mouse = false;

    for (path, mut dev) in evdev::enumerate() {
        let name = dev.name().unwrap_or("unknown").to_string();
        if name.contains("ydotool") || name.contains("albion-") || name.contains("hl-virtual") {
            continue;
        }

        let is_mouse = dev.supported_keys().map_or(false, |s| {
            s.contains(KeyCode::BTN_LEFT) && s.contains(KeyCode::BTN_RIGHT)
        });
        if !is_mouse {
            continue;
        }

        found_mouse = true;
        if debug {
            eprintln!("[DEBUG] 被動監聽鼠標: {} ({})", name, path.display());
        }

        let l = Arc::clone(&left_held);
        let r = Arc::clone(&right_held);
        thread::spawn(move || loop {
            match dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        if let EventSummary::Key(_, code, val) = ev.destructure() {
                            if code == KeyCode::BTN_LEFT {
                                if val == 1 {
                                    l.store(true, Ordering::Relaxed);
                                } else if val == 0 {
                                    l.store(false, Ordering::Relaxed);
                                }
                            } else if code == KeyCode::BTN_RIGHT {
                                if val == 1 {
                                    r.store(true, Ordering::Relaxed);
                                } else if val == 0 {
                                    r.store(false, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    l.store(false, Ordering::Relaxed);
                    r.store(false, Ordering::Relaxed);
                    break;
                }
            }
        });
    }

    if !found_mouse {
        eprintln!("未找到可用鼠標設備");
        std::process::exit(1);
    }

    let mut was_both = false;
    let mut toggling_active = false;

    loop {
        let enabled = Path::new(FLAG_FILE).exists();
        if !enabled {
            toggling_active = false;
            was_both = false;
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        let both = left_held.load(Ordering::Relaxed) && right_held.load(Ordering::Relaxed);

        let should_click = if hold_mode {
            both
        } else {
            if both && !was_both {
                toggling_active = !toggling_active;
                if debug {
                    eprintln!(
                        "[DEBUG] 雙鍵觸發切換 -> {}",
                        if toggling_active {
                            "開始連點"
                        } else {
                            "停止連點"
                        }
                    );
                }
            }
            was_both = both;
            toggling_active
        };

        if should_click {
            let key = read_mouse_key();
            let cmd = format!(
                r#"dispatch hl.dsp.send_shortcut({{ mods = "", key = "{}" }})"#,
                key
            );
            let res = send_hypr_cmd(&sock_path, &cmd);
            if debug {
                eprintln!("[DEBUG] IPC click {} -> {:?}", key, res);
            }
            thread::sleep(Duration::from_millis(80)); // 約 12 次/秒
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
}
