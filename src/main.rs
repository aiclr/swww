use clap::Parser;
use std::{path::PathBuf, process::Stdio, time::Duration};

use utils::{
    cache,
    ipc::{self, connect_to_socket, get_socket_path, read_socket, Answer, RequestSend},
};

mod imgproc;
use imgproc::*;

mod cli;
use cli::{ResizeStrategy, Swww};

fn main() -> Result<(), String> {
    let swww = Swww::parse();
    if let Swww::Init {
        no_daemon, format, ..
    } = &swww
    {
        eprintln!(
            "DEPRECATION WARNING: `swww init` IS DEPRECATED. Call `swww-daemon` directly instead"
        );
        match is_daemon_running() {
            Ok(false) => {
                let socket_path = get_socket_path();
                if socket_path.exists() {
                    eprintln!(
                        "WARNING: socket file {} was not deleted when the previous daemon exited",
                        socket_path.to_string_lossy()
                    );
                    if let Err(e) = std::fs::remove_file(socket_path) {
                        return Err(format!("failed to delete previous socket: {e}"));
                    }
                }
            }
            Ok(true) => {
                return Err("There seems to already be another instance running...".to_string())
            }
            Err(e) => {
                eprintln!("WARNING: failed to read '/proc' directory to determine whether the daemon is running: {e}
                          Falling back to trying to checking if the socket file exists...");
                let socket_path = get_socket_path();
                if socket_path.exists() {
                    return Err(format!(
                        "Found socket at {}. There seems to be an instance already running...",
                        socket_path.to_string_lossy()
                    ));
                }
            }
        }
        spawn_daemon(*no_daemon, format)?;
        if *no_daemon {
            return Ok(());
        }
    }

    if let Swww::ClearCache = &swww {
        return cache::clean().map_err(|e| format!("failed to clean the cache: {e}"));
    }

    loop {
        let socket = connect_to_socket(&get_socket_path(), 5, 100)?;
        RequestSend::Ping.send(&socket)?;
        let bytes = read_socket(&socket)?;
        let answer = Answer::receive(bytes);
        if let Answer::Ping(configured) = answer {
            if configured {
                break;
            }
        } else {
            return Err("Daemon did not return Answer::Ping, as expected".to_string());
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    process_swww_args(&swww)
}

fn process_swww_args(args: &Swww) -> Result<(), String> {
    let request = match make_request(args)? {
        Some(request) => request,
        None => return Ok(()),
    };
    let socket = connect_to_socket(&get_socket_path(), 5, 100)?;
    request.send(&socket)?;
    let bytes = read_socket(&socket)?;
    drop(socket);
    match Answer::receive(bytes) {
        Answer::Err(msg) => return Err(msg.to_string()),
        Answer::Info(info) => info.iter().for_each(|i| println!("{}", i)),
        Answer::Ok => {
            if let Swww::Kill = args {
                #[cfg(debug_assertions)]
                let tries = 20;
                #[cfg(not(debug_assertions))]
                let tries = 10;
                let socket_path = get_socket_path();
                for _ in 0..tries {
                    if !socket_path.exists() {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                return Err(format!(
                    "Could not confirm socket deletion at: {socket_path:?}"
                ));
            }
        }
        Answer::Ping(_) => {
            return Ok(());
        }
    }
    Ok(())
}

fn make_request(args: &Swww) -> Result<Option<RequestSend>, String> {
    match args {
        Swww::Clear(c) => {
            let (format, _, _) = get_format_dims_and_outputs(&[])?;
            let mut color = c.color;
            if format.must_swap_r_and_b_channels() {
                color.swap(0, 2);
            }
            let clear = ipc::ClearSend {
                color,
                outputs: split_cmdline_outputs(&c.outputs),
            };
            Ok(Some(RequestSend::Clear(clear.create_request())))
        }
        Swww::Restore(restore) => {
            let requested_outputs = split_cmdline_outputs(&restore.outputs);
            restore_from_cache(&requested_outputs)?;
            Ok(None)
        }
        Swww::ClearCache => unreachable!("there is no request for clear-cache"),
        Swww::Img(img) => {
            let requested_outputs = split_cmdline_outputs(&img.outputs);
            let (dims, outputs) = get_dimensions_and_outputs(&requested_outputs)?;
            match &img.image {
                cli::CliImage::Path(path) => {
                    let imgbuf = ImgBuf::new(path)?;
                    if imgbuf.is_animated() {
                        match std::thread::scope::<_, Result<_, String>>(|s1| {
                            let animations =
                                s1.spawn(|| make_animation_request(img, &dims, &outputs));
                            let first_frame = imgbuf
                                .into_frames()?
                                .next()
                                .ok_or("missing first frame".to_owned())?
                                .map_err(|e| format!("unable to decode first frame: {e}"))?;

                            let img_raw = frame_to_rgb(first_frame);


                            // resize imgs
                            let mut img_for_dims = Vec::with_capacity(dims.len());

                            for (dim, outputs) in dims.iter().zip(&outputs) {
                                let ipc_img = ipc::Img {
                                    img: match img.resize {
                                        ResizeStrategy::No => img_pad(img_raw.clone(), *dim, &img.fill_color)?,
                                        ResizeStrategy::Crop => img_resize_crop(img_raw.clone(), *dim, make_filter(&img.filter))?,
                                        ResizeStrategy::Fit => img_resize_fit(img_raw.clone(), *dim, make_filter(&img.filter), &img.fill_color)?,
                                    }
                                    .into_boxed_slice(),
                                    path: match path.canonicalize() {
                                        Ok(p) => p.to_string_lossy().to_string(),
                                        Err(e) => {
                                            if let Some("-") = path.to_str() {
                                                "STDIN".to_string()
                                            } else {
                                                return Err(format!("failed no canonicalize image path: {e}"));
                                            }
                                        }
                                    },
                                };
                                img_for_dims.push((ipc_img, outputs.to_owned().into_boxed_slice()));
                            }

                            let img_request = make_img_request(
                                img,
                                img_for_dims,
                            )?;

                            let animations =
                                animations.join().unwrap_or_else(|e| Err(format!("{e:?}")));

                            let socket = connect_to_socket(5, 100)?;
                            Request::Img(img_request).send(&socket)?;
                            let bytes = read_socket(&socket)?;
                            drop(socket);
                            if let ArchivedAnswer::Err(e) = Answer::receive(&bytes) {
                                return Err(format!("daemon error when sending image: {e}"));
                            }
                            animations
                        }) {
                            Ok(animations) => Ok(Some(Request::Animation(animations))),
                            Err(e) => Err(format!("failed to create animated request: {e}")),
                        }
                    } else {
                        let img_raw = imgbuf.decode()?;
                        let mut img_for_dims = Vec::with_capacity(dims.len());
                        for (dim, outputs) in dims.iter().zip(&outputs) {
                            let ipc_img = ipc::Img {
                                img: match img.resize {
                                    ResizeStrategy::No => img_pad(img_raw.clone(), *dim, &img.fill_color)?,
                                    ResizeStrategy::Crop => img_resize_crop(img_raw.clone(), *dim, make_filter(&img.filter))?,
                                    ResizeStrategy::Fit => img_resize_fit(img_raw.clone(), *dim, make_filter(&img.filter), &img.fill_color)?,
                                }
                                .into_boxed_slice(),
                                path: match path.canonicalize() {
                                    Ok(p) => p.to_string_lossy().to_string(),
                                    Err(e) => {
                                        if let Some("-") = path.to_str() {
                                            "STDIN".to_string()
                                        } else {
                                            return Err(format!("failed no canonicalize image path: {e}"));
                                        }
                                    }
                                },
                            };
                            img_for_dims.push((ipc_img, outputs.to_owned().into_boxed_slice()));
                        }
                        Ok(Some(Request::Img(make_img_request(
                            img,
                            img_for_dims,
                        )?)))
                    }
                }
                cli::CliImage::Color(color) => {
                    
                    let mut img_for_dims = Vec::with_capacity(dims.len());
                    
                    for (dim, outputs) in dims.iter().zip(&outputs) {
                        let ipc_img = ipc::Img {
                            img: image::RgbImage::from_pixel(dim.0, dim.1, image::Rgb(*color))
                            .to_vec()
                            .into_boxed_slice(),
                            path: format!("0x{:02x}{:02x}{:02x}", color[0], color[1], color[2]),
                        };
                        img_for_dims.push((ipc_img, outputs.to_owned().into_boxed_slice()));
                    }

                    Ok(Some(Request::Img(make_img_request(
                        img,
                        img_for_dims,
                    )?)))
                },
            }
        }
        Swww::Init { no_cache, .. } => {
            if !*no_cache {
                restore_from_cache(&[])?;
            }
            Ok(None)
        }
        Swww::Kill => Ok(Some(RequestSend::Kill)),
        Swww::Query => Ok(Some(RequestSend::Query)),
    }
}

fn make_img_request(
    img: &cli::Img,
    img_for_dims: Vec<(ipc::Img, Box<[String]>)>,
) -> Result<ipc::ImageRequest, String> {
    let transition = make_transition(img);
    Ok((transition, img_for_dims.into_boxed_slice()))
}

#[allow(clippy::type_complexity)]
fn get_format_dims_and_outputs(
    requested_outputs: &[String],
) -> Result<(ipc::PixelFormat, Vec<(u32, u32)>, Vec<Vec<String>>), String> {
    let mut outputs: Vec<Vec<String>> = Vec::new();
    let mut dims: Vec<(u32, u32)> = Vec::new();
    let mut imgs: Vec<ipc::BgImg> = Vec::new();

    let socket = connect_to_socket(&get_socket_path(), 5, 100)?;
    RequestSend::Query.send(&socket)?;
    let bytes = read_socket(&socket)?;
    drop(socket);
    let answer = Answer::receive(bytes);
    match answer {
        Answer::Info(infos) => {
            let mut format = ipc::PixelFormat::Xrgb;
            for info in infos.iter() {
                format = info.pixel_format;
                let info_img = &info.img;
                let name = info.name.to_string();
                if !requested_outputs.is_empty() && !requested_outputs.contains(&name) {
                    continue;
                }
                let real_dim = info.real_dim();
                if let Some((_, output)) = dims
                    .iter_mut()
                    .zip(&imgs)
                    .zip(&mut outputs)
                    .find(|((dim, img), _)| real_dim == **dim && info_img == *img)
                {
                    output.push(name);
                } else {
                    outputs.push(vec![name]);
                    dims.push(real_dim);
                    imgs.push(info_img.clone());
                }
            }
            if outputs.is_empty() {
                Err("none of the requested outputs are valid".to_owned())
            } else {
                Ok((format, dims, outputs))
            }
        }
        Answer::Err(e) => Err(format!("daemon error when sending query: {e}")),
        _ => unreachable!(),
    }
}

fn make_animation_request(
    img: &cli::Img,
    dims: &[(u32, u32)],
    outputs: &[Vec<String>],
) -> Result<AnimationRequest, String> {
    match &img.image {
        cli::CliImage::Path(path) => {
            let filter = make_filter(&img.filter);
            let mut animations = Vec::with_capacity(dims.len());
            for (dim, outputs) in dims.iter().zip(outputs) {
                //TODO: make cache work for all resize strategies
                if img.resize == ResizeStrategy::Crop {
                    match cache::load_animation_frames(path, *dim) {
                        Ok(Some(animation)) => {
                            animations.push((animation, outputs.to_owned().into_boxed_slice()));
                            continue;
                        }
                        Ok(None) => (),
                        Err(e) => eprintln!("Error loading cache for {:?}: {e}", path),
                    }
                }

                let imgbuf = ImgBuf::new(path)?;
                let animation = ipc::Animation {
                    path: path.to_string_lossy().to_string(),
                    dimensions: *dim,
                    animation: compress_frames(
                        imgbuf.into_frames()?,
                        *dim,
                        filter,
                        img.resize,
                        &img.fill_color,
                    )?
                    .into_boxed_slice(),
                };
                animations.push((animation, outputs.to_owned().into_boxed_slice()));
            }
            Ok(animations.into_boxed_slice())
        }
        cli::CliImage::Color(color) => Err(format!(
            "colors are not supported for animations: {:?}",
            color
        )),
    }
}
fn split_cmdline_outputs(outputs: &str) -> Box<[String]> {
    outputs
        .split(',')
        .map(|s| s.to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn spawn_daemon(no_daemon: bool, format: &Option<cli::PixelFormat>) -> Result<(), String> {
    let mut cmd = std::process::Command::new("swww-daemon");

    if let Some(format) = format {
        cmd.arg("--format");
        cmd.arg(match format {
            cli::PixelFormat::Xrgb => "xrgb",
            cli::PixelFormat::Xbgr => "xbgr",
            cli::PixelFormat::Rgb => "rgb",
            cli::PixelFormat::Bgr => "bgr",
        });
    }

    if no_daemon {
        match cmd.status() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("error spawning swww-daemon: {e}")),
        }
    } else {
        match cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("error spawning swww-daemon: {e}")),
        }
    }
}

fn is_daemon_running() -> Result<bool, String> {
    let socket = match connect_to_socket(&get_socket_path(), 5, 100) {
        Ok(s) => s,
        // likely a connection refused; either way, this is a reliable signal there's no surviving
        // daemon.
        Err(_) => return Ok(false),
    };

    RequestSend::Ping.send(&socket)?;
    let answer = Answer::receive(read_socket(&socket)?);
    match answer {
        Answer::Ping(_) => Ok(true),
        _ => Err("Daemon did not return Answer::Ping, as expected".to_string()),
    }
}

fn restore_from_cache(requested_outputs: &[String]) -> Result<(), String> {
    let (_, _, outputs) = get_format_dims_and_outputs(requested_outputs)?;

    for output in outputs.iter().flatten() {
        let img_path = utils::cache::get_previous_image_path(output)
            .map_err(|e| format!("failed to get previous image path: {e}"))?;
        #[allow(deprecated)]
        if let Err(e) = process_swww_args(&Swww::Img(cli::Img {
            image: cli::parse_image(&img_path)?,
            outputs: output.to_string(),
            no_resize: false,
            resize: ResizeStrategy::Crop,
            fill_color: [0, 0, 0],
            filter: cli::Filter::Lanczos3,
            transition_type: cli::TransitionType::None,
            transition_step: std::num::NonZeroU8::MAX,
            transition_duration: 0.0,
            transition_fps: 30,
            transition_angle: 0.0,
            transition_pos: cli::CliPosition {
                x: cli::CliCoord::Pixel(0.0),
                y: cli::CliCoord::Pixel(0.0),
            },
            invert_y: false,
            transition_bezier: (0.0, 0.0, 0.0, 0.0),
            transition_wave: (0.0, 0.0),
        })) {
            eprintln!("WARNING: failed to load cache for output {output}: {e}");
        }
    }

    Ok(())
}
