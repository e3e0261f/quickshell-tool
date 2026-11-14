use evdev::{EventSummary, KeyCode};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

const FLAG_FILE: &str = "/tmp/q_spam_enabled";

const CMD_SHORTCUT: &str = r#"dispatch hl.dsp.send_shortcut({ mods = "", key = "q" })"#;
const CMD_DOWN: &str = r#"dispatch hl.dsp.send_key_state({ state = "down", mods = "", key = "q", window = "activewindow" })"#;
const CMD_UP: &str = r#"dispatch hl.dsp.send_key_state({ state = "up", mods = "", key = "q", window = "activewindow" })"#;

fn mod_bit(code: KeyCode) -> u8 {
    match code {
        KeyCode::KEY_LEFTCTRL => 1 << 0,
        KeyCode::KEY_RIGHTCTRL => 1 << 1,
        KeyCode::KEY_LEFTALT => 1 << 2,
        KeyCode::KEY_RIGHTALT => 1 << 3,
        KeyCode::KEY_LEFTMETA => 1 << 4,
        KeyCode::KEY_RIGHTMETA => 1 << 5,
        _ => 0,
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
    let use_key_state = args.iter().any(|a| a == "--key-state");

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
            if use_key_state {
                "send_key_state"
            } else {
                "send_shortcut"
            }
        );
    }

    // 被動監聽物理鍵盤（不 grab、不建虛擬設備）
    let mut found_kbd = false;
    let q_held = Arc::new(AtomicBool::new(false));
    let mod_mask = Arc::new(AtomicU8::new(0));

    for (path, mut dev) in evdev::enumerate() {
        let name = dev.name().unwrap_or("unknown").to_string();
        if name.contains("ydotool")
            || name.contains("hl-virtual-keyboard")
            || name.contains("albion-qspam")
        {
            continue;
        }

        let is_pure_kbd = dev.supported_keys().map_or(false, |s| {
            s.contains(KeyCode::KEY_Q)
                && s.contains(KeyCode::KEY_ENTER)
                && !s.contains(KeyCode::BTN_LEFT)
        });
        if !is_pure_kbd {
            continue;
        }

        found_kbd = true;
        if debug {
            eprintln!("[DEBUG] 被動監聽鍵盤: {} ({})", name, path.display());
        }

        let held = Arc::clone(&q_held);
        let mods = Arc::clone(&mod_mask);
        thread::spawn(move || loop {
            match dev.fetch_events() {
                Ok(events) => {
                    for ev in events {
                        if let EventSummary::Key(_, code, val) = ev.destructure() {
                            if code == KeyCode::KEY_Q {
                                match val {
                                    1 => held.store(true, Ordering::Relaxed),
                                    0 => held.store(false, Ordering::Relaxed),
                                    _ => {}
                                }
                                if debug && val != 2 {
                                    eprintln!(
                                        "[DEBUG] [{}] KEY_Q val={} | mod={:#04x} | flag={}",
                                        name,
                                        val,
                                        mods.load(Ordering::Relaxed),
                                        Path::new(FLAG_FILE).exists()
                                    );
                                }
                            } else {
                                let bit = mod_bit(code);
                                if bit != 0 {
                                    if val == 1 {
                                        mods.fetch_or(bit, Ordering::Relaxed);
                                    } else if val == 0 {
                                        mods.fetch_and(!bit, Ordering::Relaxed);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    held.store(false, Ordering::Relaxed);
                    mods.store(0, Ordering::Relaxed);
                    break;
                }
            }
        });
    }

    if !found_kbd {
        eprintln!("未找到可用物理鍵盤設備");
        std::process::exit(1);
    }

    // 主循環：物理按下第 1 下由系統原生發出；按住 >= 130ms 後開始通過 Hyprland Socket 連發
    let mut held_since: Option<Instant> = None;
    let mut was_spamming = false;

    loop {
        let q = q_held.load(Ordering::Relaxed);
        let m = mod_mask.load(Ordering::Relaxed);
        let f = Path::new(FLAG_FILE).exists();

        if q && m == 0 && f {
            let start = *held_since.get_or_insert_with(Instant::now);
            if start.elapsed() >= Duration::from_millis(130) {
                if debug && !was_spamming {
                    eprintln!("[DEBUG] >>> 開始連發 Q (Hyprland IPC)");
                    was_spamming = true;
                }

                if use_key_state {
                    let r1 = send_hypr_cmd(&sock_path, CMD_DOWN);
                    thread::sleep(Duration::from_millis(25));
                    let r2 = send_hypr_cmd(&sock_path, CMD_UP);
                    if debug {
                        eprintln!("[DEBUG] IPC key_state -> down={:?}, up={:?}", r1, r2);
                    }
                    thread::sleep(Duration::from_millis(20));
                } else {
                    let r = send_hypr_cmd(&sock_path, CMD_SHORTCUT);
                    if debug {
                        eprintln!("[DEBUG] IPC shortcut -> {:?}", r);
                    }
                    thread::sleep(Duration::from_millis(40));
                }
            } else {
                thread::sleep(Duration::from_millis(5));
            }
        } else {
            if debug && was_spamming {
                eprintln!("[DEBUG] <<< 停止連發");
                was_spamming = false;
            }
            held_since = None;
            thread::sleep(Duration::from_millis(5));
        }
    }
}
