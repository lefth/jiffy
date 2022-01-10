use std::{env, sync::Arc};

use anyhow::Result;
use clap::StructOpt;
use log::LevelFilter;

use jiffy::{encode_videos, Args, ENCODE_DIR};

#[tokio::main]
async fn main() -> Result<()> {
    if env::var_os("RUST_LOG").is_some() {
        env_logger::init();
    } else {
        env_logger::builder().filter_level(LevelFilter::Info).init();
    }

    let mut args = Args::parse();
    if args
        .video_root
        .components()
        .last()
        .expect("Must use a valid path as the video root directory")
        .as_os_str()
        == ENCODE_DIR
    {
        args.video_root.pop();
    }
    encode_videos(&Arc::new(args)).await?;
    Ok(())
}
