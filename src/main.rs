// #![windows_subsystem = "windows"]

mod config {
    use device_query::Keycode;
    use std::sync::Mutex;

    pub static KEY: Mutex<Keycode> = Mutex::new(Keycode::F10);
    pub static FPS: Mutex<i32> = Mutex::new(60);
    pub static KBPS: Mutex<i32> = Mutex::new(10000);
    pub static TIME: Mutex<i32> = Mutex::new(10);
    pub static ENCODER: Mutex<i32> = Mutex::new(0);
}

use device_query::{DeviceQuery, DeviceState, Keycode};
use dxgi_capture_rs::DXGIManager;
use std::collections::HashMap;
use std::fs::{self, File, remove_file};
use std::io::{BufWriter, Write};
use std::os::windows::process::CommandExt;
use std::process::{self, Command, Stdio};
use std::str::FromStr;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};
use time::{OffsetDateTime, format_description};
use tinyjson::JsonValue;
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem},
};
use win_msgbox::Okay;
use winit::event_loop::{ControlFlow, EventLoop};

const DEFAULT_CFG: &str = "{\"time\":10,\"fps\":60,\"kbps\":10000,\"key\":\"F10\", \"encoder\": 0}";

fn load_settings() -> Result<(), Box<dyn std::error::Error>> {
    let cfg_string = fs::read_to_string("ack.cfg").unwrap_or_else(|_| {
        let _ = fs::write("ack.cfg", DEFAULT_CFG);
        String::from(DEFAULT_CFG)
    });

    let parsed: JsonValue = cfg_string.parse()?;
    let map: &HashMap<String, JsonValue> = parsed.get().ok_or("Invalid JSON root")?;

    let get_num = |k| {
        map.get(k)
            .and_then(|v| v.get::<f64>())
            .copied()
            .ok_or(format!("Missing/Invalid {}", k))
    };
    let get_str = |k| {
        map.get(k)
            .and_then(|v| v.get::<String>())
            .cloned()
            .ok_or(format!("Missing/Invalid {}", k))
    };

    let time = get_num("time")?;
    let fps = get_num("fps")?;
    let kbps = get_num("kbps")? * 1000.0;
    let encoder = get_num("encoder")?;
    let key = get_str("key")?;

    *crate::config::ENCODER.lock().unwrap() = encoder.clamp(0., 3.) as i32;
    *crate::config::KEY.lock().unwrap() = Keycode::from_str(&key)?;
    *crate::config::KBPS.lock().unwrap() = kbps as i32;
    *crate::config::FPS.lock().unwrap() = fps as i32;
    *crate::config::TIME.lock().unwrap() = time as i32;

    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        use windows::Win32::Media::timeBeginPeriod;
        timeBeginPeriod(1);
    }

    if let Err(e) = load_settings() {
        let _ = win_msgbox::information::<Okay>(&format!("{}\nResetting config.", e))
            .title("Configuration Error")
            .show();
        let _ = fs::write("ack.cfg", DEFAULT_CFG);
        load_settings()?;
    }

    let manager = DXGIManager::new(100)?;
    let (width, height) = manager.geometry();
    let device_state = DeviceState::new();

    let _ = remove_file("buffer1.mp4");
    let _ = remove_file("buffer0.mp4");

    let (tx, rx) = mpsc::channel();

    let tray_menu = Menu::new();
    let quit_btn = MenuItem::new("Quit", true, None);
    tray_menu.append(&quit_btn).unwrap();
    let quit_id = quit_btn.into_id();

    let icon_data = include_bytes!("../icon.raw").to_vec();
    let tray_icon_icon = Icon::from_rgba(icon_data, 16, 16).unwrap();
    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("acid's clipping kit")
        .with_icon(tray_icon_icon)
        .build()
        .unwrap();

    let menu_channel = MenuEvent::receiver();
    let event_loop = EventLoop::new().unwrap();

    thread::spawn(move || {
        if let Err(e) = recording_loop(rx, width as u32, height as u32, manager, device_state) {
            let _ = win_msgbox::error::<Okay>(&e.to_string())
                .title("Fatal Error")
                .show();
            process::exit(1);
        }
    });

    #[allow(deprecated)]
    event_loop
        .run(move |_event, event_loop| {
            event_loop.set_control_flow(ControlFlow::Poll);
            if let Ok(event) = menu_channel.try_recv() {
                if event.id == quit_id {
                    let _ = tx.send(true);
                    event_loop.exit();
                }
            }
        })
        .unwrap();

    Ok(())
}

fn recording_loop(
    rx: Receiver<bool>,
    width: u32,
    height: u32,
    mut manager: DXGIManager,
    device_state: DeviceState,
) -> Result<(), Box<dyn std::error::Error>> {
    let fps = *crate::config::FPS.lock().unwrap();
    let kbps = *crate::config::KBPS.lock().unwrap();
    let key_code = *crate::config::KEY.lock().unwrap();
    let time_seg = *crate::config::TIME.lock().unwrap();
    let enc_idx = *crate::config::ENCODER.lock().unwrap();

    let encoder = match enc_idx {
        0 => "libx264",
        1 => "h264_amf",
        2 => "h264_nvenc",
        3 => "h264_qsv",
        _ => "libx264",
    };

    let frame_duration = Duration::from_nanos((1_000_000_000 / fps) as u64);

    'main_loop: loop {
        let mut child = Command::new("./ffmpeg.exe")
            .args([
                "-y",
                "-f",
                "rawvideo",
                "-vcodec",
                "rawvideo",
                "-pixel_format",
                "bgra",
                "-video_size",
                &format!("{}x{}", width, height),
                "-framerate",
                &fps.to_string(),
                "-i",
                "-",
                "-c:v",
                encoder,
                "-tune",
                "zerolatency",
                "-b:v",
                &format!("{}k", kbps / 1000),
                "-pix_fmt",
                "yuv420p",
                "-f",
                "segment",
                "-segment_time",
                &time_seg.to_string(),
                "-segment_wrap",
                "2",
                "-reset_timestamps",
                "1",
                "buffer%d.mp4",
            ])
            .creation_flags(0x08000000)
            .stdin(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let mut stdin =
            BufWriter::with_capacity((width * height * 4) as usize, child.stdin.take().unwrap());

        let mut next_frame_time = Instant::now();
        let mut last_ckey_state = false;

        loop {
            let now = Instant::now();
            if now < next_frame_time {
                let diff = next_frame_time - now;
                if diff > Duration::from_millis(1) {
                    thread::sleep(diff - Duration::from_millis(1));
                }
                while Instant::now() < next_frame_time {}
            }
            next_frame_time += frame_duration;

            if let Ok((data, _)) = manager.capture_frame() {
                let slice = unsafe {
                    std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4)
                };

                if stdin.write_all(slice).is_err() {
                    break;
                }
            }

            let keys = device_state.get_keys();
            if keys.contains(&Keycode::F1) && keys.contains(&Keycode::LControl) {
                return Ok(());
            }

            if rx.try_recv().is_ok() {
                break 'main_loop;
            }

            let capture_pressed = keys.contains(&key_code);
            if capture_pressed && !last_ckey_state {
                break;
            }
            last_ckey_state = capture_pressed;
        }

        drop(stdin);
        let _ = child.wait();
        save_final_clip()?;
    }
    Ok(())
}

fn save_final_clip() -> Result<(), Box<dyn std::error::Error>> {
    beep(1000);

    let b0 = std::fs::metadata("buffer0.mp4");
    let b1 = std::fs::metadata("buffer1.mp4");
    let mut list = Vec::new();

    match (b0, b1) {
        (Ok(m0), Ok(m1)) => {
            if m0.modified()? < m1.modified()? {
                list.push("buffer0.mp4");
                list.push("buffer1.mp4");
            } else {
                list.push("buffer1.mp4");
                list.push("buffer0.mp4");
            }
        }
        (Ok(_), Err(_)) => list.push("buffer0.mp4"),
        (Err(_), Ok(_)) => list.push("buffer1.mp4"),
        _ => return Ok(()),
    }

    let list_path = "concat_list.txt";
    {
        let mut f = File::create(list_path)?;
        for file in list {
            writeln!(f, "file '{}'", file)?;
        }
    }

    beep(2000);

    let now = OffsetDateTime::now_utc();
    let fmt = format_description::parse("[year]-[month]-[day].[hour]_[minute]_[second]")?;
    let output_name = format!("clip_{}.mp4", now.format(&fmt)?);

    Command::new("./ffmpeg.exe")
        .args([
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            list_path,
            "-c",
            "copy",
            &output_name,
        ])
        .creation_flags(0x08000000)
        .status()?;

    let _ = std::fs::remove_file(list_path);
    beep(3000);
    Ok(())
}

fn beep(freq: u32) {
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::Beep;
        let _ = Beep(freq, 50);
    }
}
