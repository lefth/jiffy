use std::{env, thread, time::Duration};

use anyhow::{bail, Result};
use clap::Parser;
#[allow(unused_imports)]
use log::*;

use jiffy::{get_output_dir, Args, Encoder};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.quiet as i8 - args.verbose as i8 {
        ..-1 => {
            env_logger::builder()
                .filter_level(LevelFilter::Trace)
                .init();
        }
        -1 => {
            env_logger::builder()
                .filter_level(LevelFilter::Debug)
                .init();
        }
        0 => {
            if env::var_os("RUST_LOG").is_some() {
                env_logger::init();
            } else {
                env_logger::builder().filter_level(LevelFilter::Info).init();
            }
        }
        1 => {
            env_logger::builder().filter_level(LevelFilter::Warn).init();
        }
        2.. => {
            env_logger::builder()
                .filter_level(LevelFilter::Error)
                .init();
        }
    }

    if !args.video_root.exists() {
        bail!("Video root does not exist: {:?}", args.video_root);
    }

    if let Ok(video_root) = args.video_root.canonicalize() {
        if video_root
            .components()
            .last()
            .expect("Cannot get components of encode path")
            .as_os_str()
            == get_output_dir(&args)
        {
            warn!(
                "The video directory is named {:?}. Did you mean to encode the parent directory?",
                args.output_dir
            );
            thread::sleep(Duration::from_millis(2000));
        }
    }
    Encoder::new(args)?.encode_videos().await
}
