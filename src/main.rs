use std::{env, thread, time::Duration};

use anyhow::{bail, Result};
use clap::Parser;
#[allow(unused_imports)]
use log::*;

use jiffy::{get_output_dir, Cli, Encoder};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.get_verbosity() {
        2.. => {
            env_logger::builder()
                .filter_level(LevelFilter::Trace)
                .init();
        }
        1 => {
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
        -1 => {
            env_logger::builder().filter_level(LevelFilter::Warn).init();
        }
        ..-1 => {
            env_logger::builder()
                .filter_level(LevelFilter::Error)
                .init();
        }
    }

    if !cli.video_root.exists() {
        bail!("Video root does not exist: {:?}", cli.video_root);
    }

    if let Ok(video_root) = cli.video_root.canonicalize() {
        if video_root
            .components()
            .last()
            .expect("Cannot get components of encode path")
            .as_os_str()
            == get_output_dir(&cli)
        {
            warn!(
                "The video directory is named {:?}. Did you mean to encode the parent directory?",
                cli.output_dir
            );
            thread::sleep(Duration::from_millis(2000));
        }
    }
    Encoder::new(cli)?.encode_videos().await
}
