//! Command line options.

use std::env;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// TCP port on 127.0.0.1; the tablet reaches it through `adb reverse`.
    pub port: u16,
    /// Frames per second sent to the tablet.
    pub fps: u32,
    /// Video bitrate in kbps.
    pub bitrate: u32,
    /// Frames between keyframes.
    pub keyframe_interval: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { port: 9999, fps: 60, bitrate: 8000, keyframe_interval: 10 }
    }
}

impl ServerConfig {
    pub fn from_args() -> Self {
        let mut config = Self::default();
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = |current: u32| args.next().and_then(|v| v.parse().ok()).unwrap_or(current);
            match arg.as_str() {
                "-p" | "--port" => config.port = value(config.port as u32) as u16,
                "-f" | "--fps" => config.fps = value(config.fps).max(1),
                "-b" | "--bitrate" => config.bitrate = value(config.bitrate),
                "-k" | "--keyframe-interval" => config.keyframe_interval = value(config.keyframe_interval),
                "-h" | "--help" => {
                    println!(
                        "mmspad: use an Android tablet over USB as a pen tablet and touchpad\n\n\
                         USAGE: mmspad [OPTIONS]\n\n\
                         OPTIONS:\n  \
                         -p, --port <PORT>            TCP port on 127.0.0.1 [default: 9999]\n  \
                         -f, --fps <FPS>              Frames per second [default: 60]\n  \
                         -b, --bitrate <KBPS>         Video bitrate [default: 8000]\n  \
                         -k, --keyframe-interval <N>  Frames between keyframes [default: 10]"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("mmspad: unknown option {other}"),
            }
        }
        config
    }
}
