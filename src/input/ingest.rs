use std::{
    io::{BufReader, Error, Read},
    path::Path,
    process::{Command, Stdio},
    sync::atomic::Ordering,
    thread,
};

use crossbeam_channel::Sender;
use simplelog::*;

use crate::utils::{stderr_reader, GlobalConfig, Ingest, ProcessControl};

/// Overlay Filter
///
/// When a logo is set, we create here the filter for the server.
fn overlay(config: &GlobalConfig) -> String {
    let mut logo_chain = String::new();

    if config.processing.add_logo && Path::new(&config.processing.logo).is_file() {
        let opacity = format!(
            "format=rgba,colorchannelmixer=aa={}",
            config.processing.logo_opacity
        );
        let logo_loop = "loop=loop=-1:size=1:start=0";
        logo_chain = format!("[v];movie={},{logo_loop},{opacity}", config.processing.logo);

        logo_chain
            .push_str(format!("[l];[v][l]{}:shortest=1", config.processing.logo_filter).as_str());
    }

    logo_chain
}

/// Audio Filter
///
/// If needed we add audio filters to the server instance.
fn audio_filter(config: &GlobalConfig) -> String {
    let mut audio_chain = ";[0:a]afade=in:st=0:d=0.5".to_string();

    if config.processing.add_loudnorm {
        audio_chain.push_str(
            format!(
                ",loudnorm=I={}:TP={}:LRA={}",
                config.processing.loud_i, config.processing.loud_tp, config.processing.loud_lra
            )
            .as_str(),
        );
    }

    if config.processing.volume != 1.0 {
        audio_chain.push_str(format!(",volume={}", config.processing.volume).as_str());
    }

    audio_chain.push_str("[aout1]");

    audio_chain
}

/// ffmpeg Ingest Server
///
/// Start ffmpeg in listen mode, and wait for input.
pub fn ingest_server(
    log_format: String,
    ingest_sender: Sender<(usize, [u8; 65088])>,
    mut proc_control: ProcessControl,
) -> Result<(), Error> {
    let config = GlobalConfig::global();
    let mut buffer: [u8; 65088] = [0; 65088];
    let mut filter = format!(
        "[0:v]fps={},scale={}:{},setdar=dar={},fade=in:st=0:d=0.5",
        config.processing.fps,
        config.processing.width,
        config.processing.height,
        config.processing.aspect
    );

    filter.push_str(&overlay(config));
    filter.push_str("[vout1]");
    filter.push_str(audio_filter(config).as_str());
    let mut filter_list = vec![
        "-filter_complex",
        &filter,
        "-map",
        "[vout1]",
        "-map",
        "[aout1]",
    ];

    let mut server_cmd = vec!["-hide_banner", "-nostats", "-v", log_format.as_str()];
    let stream_input = config.ingest.input_cmd.clone().unwrap();
    let stream_settings = config.processing.settings.clone().unwrap();

    server_cmd.append(&mut stream_input.iter().map(String::as_str).collect());
    server_cmd.append(&mut filter_list);
    server_cmd.append(&mut stream_settings.iter().map(String::as_str).collect());

    let mut is_running;

    info!(
        "Start ingest server, listening on: <b><magenta>{}</></b>",
        stream_input.last().unwrap()
    );

    debug!(
        "Server CMD: <bright-blue>\"ffmpeg {}\"</>",
        server_cmd.join(" ")
    );

    'ingest_iter: loop {
        let mut server_proc = match Command::new("ffmpeg")
            .args(server_cmd.clone())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Err(e) => {
                error!("couldn't spawn ingest server: {e}");
                panic!("couldn't spawn ingest server: {e}")
            }
            Ok(proc) => proc,
        };
        let mut ingest_reader = BufReader::new(server_proc.stdout.take().unwrap());
        let server_err = BufReader::new(server_proc.stderr.take().unwrap());
        let error_reader_thread = thread::spawn(move || stderr_reader(server_err, "Server"));

        *proc_control.server_term.lock().unwrap() = Some(server_proc);
        is_running = false;

        loop {
            let bytes_len = match ingest_reader.read(&mut buffer[..]) {
                Ok(length) => length,
                Err(e) => {
                    debug!("Ingest server read {e:?}");
                    break;
                }
            };

            if !is_running {
                proc_control.server_is_running.store(true, Ordering::SeqCst);
                is_running = true;
            }

            if bytes_len > 0 {
                if let Err(e) = ingest_sender.send((bytes_len, buffer)) {
                    error!("Ingest server write error: {e:?}");

                    proc_control.is_terminated.store(true, Ordering::SeqCst);
                    break 'ingest_iter;
                }
            } else {
                break;
            }
        }

        drop(ingest_reader);
        proc_control.server_is_running.store(false, Ordering::SeqCst);

        if let Err(e) = proc_control.wait(Ingest) {
            error!("{e}")
        }

        if let Err(e) = error_reader_thread.join() {
            error!("{e:?}");
        };

        if proc_control.is_terminated.load(Ordering::SeqCst) {
            break;
        }
    }

    Ok(())
}
