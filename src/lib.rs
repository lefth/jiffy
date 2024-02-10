use std::{
    cmp::max, collections::VecDeque, env, ffi::OsString, io::Write, path::{Path, PathBuf}, pin::Pin, sync::{
        mpsc::{channel, Sender},
        Arc,
    }, time::Duration
};

use lexical_sort;
use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use futures::stream::{FuturesUnordered, StreamExt};
use globset::{Glob, GlobSetBuilder};
use log::warn;
use regex::Regex;

pub const ENCODE_DIR: &str = "encoded";

pub mod input_file;
pub use input_file::*;
pub mod logger;
#[allow(unused_imports)]
pub use logger::*;
use tokio::{io::AsyncReadExt, process::Command, select, time::sleep};

#[derive(PartialEq, std::fmt::Debug)]
enum Codec {
    Av1,
    H265,
    H264,
    Copy,
}

#[derive(Parser)]
pub struct Args {
    /// Set the quality level (for either encoded). The default is 24 for AV1 and 22 for H265, but
    /// if unspecified, a better CRF may be used for small videos, or a lower quality CRF may be
    /// used for animation.
    #[clap(long)]
    crf: Option<u8>,

    /// Use x265 instead of aom-av1. This is true by default with --animation.
    #[clap(long, alias = "h265", conflicts_with_all = ["av1", "reference"])]
    x265: bool,

    /// Use x264 to make a high quality (high disk space) fast encode.
    #[clap(long, conflicts_with_all = ["av1", "x265"])]
    reference: bool,

    /// Use libaom-av1 for encoding. This is the default, except for animation.
    #[clap(long = "av1", aliases = ["aom", "libaom", "aom-av1"])]
    av1: bool,

    /// Use settings that work well for anime or animation.
    #[clap(long = "animation", alias = "anime",
        default_value_ifs = [
            ("anime_slow_well_lit", "true", "true"),
            ("anime_mixed_dark_battle", "true", "true"),
        ],
    )]
    anime: bool,

    /// Use this setting for slow well lit anime, like slice of life:
    #[clap(long, conflicts_with_all = ["av1", "anime_mixed_dark_battle", "reference"])]
    anime_slow_well_lit: bool,

    /// Use this setting for anime with some dark scenes, some battle scenes (shonen, historical, etc.)
    #[clap(long, conflicts_with_all = ["av1", "anime_slow_well_lit", "reference"])]
    anime_mixed_dark_battle: bool,

    /// Encode this many videos in parallel. The default varies per encoder.
    #[clap(long, short, alias = "max-jobs")]
    jobs: Option<usize>,

    /// Encode as 720p. Otherwise the video will be 1080p. The source size is taken into
    /// consideration; in no case is a video scaled up.
    #[clap(long = "720p")]
    height_720p: bool,

    /// Encode as 8-bit. Otherwise the video will be 10-bit, except if creating
    /// a file as reference or for TV.
    #[clap(
        long = "8-bit",
        alias = "8bit",
        default_value_if("for_tv", "true", "true"),
        default_value_if("reference", "true", "true")
    )]
    pub eight_bit: bool,

    /// The encoding preset to use--by default this is fairly slow. By default, "5" for libaom,
    /// "slow" for x265.
    #[clap(
        long,
        hide_default_value = true,
        default_value = "5",
        default_value_if("reference", "true", "veryfast"),
        default_value_if("x265", "true", "slow"),
        default_value_if("for_tv", "true", "fast")
    )]
    pub preset: String,

    /// Overwrite existing output files
    #[clap(long)]
    pub overwrite: bool,

    /// Add additional ffmpeg flags, such as "-to 5:00" to quickly test the first few minutes of a
    /// file.  Each option should be passed separately, for example:
    /// `jiffy --extra-flag='-ss 30' --extra-flag='-t 5:00'`
    #[clap(long, allow_hyphen_values(true))]
    pub extra_flag: Vec<String>,

    /// Don't write log files for each ffmpeg invocation. This avoids polluting your output
    /// directory with a log file per input.
    #[clap(long, short)]
    pub no_log: bool,

    /// Don't check if the audio streams are within acceptable limits--just reencode them (unless
    /// `--copy-audio` was specified). This saves a little time in some circumstances.
    #[clap(
        long = "skip-bitrate-check",
        default_value_if("copy_streams", "true", "true"),
        default_value_if("no_audio", "true", "true")
    )]
    pub skip_audio_bitrate_check: bool,

    /// Keep the audio stream unchanged. This is useful if audio bitrate can't be determined.
    #[clap(long = "copy-audio", default_value_if("copy_streams", "true", "true"))]
    pub copy_audio: bool,

    /// Copy audio and video streams (don't encode). Used for testing, for example passing
    /// `--copy-streams --extra-flag='-to 30'` would copy a 30 second from each video. Implies
    /// `--copy-audio`.
    #[clap(long = "copy-streams", conflicts_with_all = ["av1", "x265", "reference", "for_tv", "height_720p",
        "anime", "anime_mixed_dark_battle", "anime_slow_well_lit", "crf", "preset"])]
    pub copy_streams: bool,

    /// For testing and benchmarking.
    #[clap(long = "no-audio", conflicts_with = "copy_audio")]
    pub no_audio: bool,

    /// Encode the videos in this directory. By default, encode in the current directory. Output
    /// files are put in "video_root/encoded". If the given path ends in "encoded", the real video
    /// root is taken to be the parent directory.
    #[clap(default_value = ".", hide_default_value = true)]
    pub video_root: PathBuf,

    /// Paths (usually glob patterns) that can be excluded. They match from the video encode root.
    /// For example, "*S01*/*E01*" might be used to skip the first episode of a TV show, and
    /// "**/*E01*" would skip the first episode of each season. This argument must be given once
    /// per exclude pattern.  See the `--include` option.
    #[clap(long)]
    pub exclude: Vec<String>,

    /// Paths (usually glob patterns) to be included; all others are excluded. They match from the
    /// video encode root. If `--include` and `--exclude` are both given, only those that are
    /// matched by the include globs and not matched by the exclude globs will be encoded.  See the
    /// `--exclude` option.
    #[clap(long)]
    pub include: Vec<String>,

    /// Run ffmpeg without `-map 0`. This occasionally fixes an encoding error.
    #[clap(long, default_value_if("for_tv", "true", "true"))]
    pub no_map_0: bool,

    /// Encode a certain number of files, then stop.
    #[clap(long)]
    pub limit: Option<usize>,

    /// Make a high quality but inefficient file for low spec televisions. The output is intended
    /// for watching, not for archival purposes. This is the only option that encodes with x264.
    /// Subtitles are hard-coded if available. These files should be compatible with Chromecast
    /// without the need for transcoding.
    ///
    // If you find this option is not compatible with your TV, please let me know what model and what encoding
    // options do work.
    #[clap(long = "for-tv", conflicts_with_all = ["av1", "x265", "reference", "anime", "anime_slow_well_lit", "anime_mixed_dark_battle"])]
    pub for_tv: bool,

    /// If a certain size reduction is expected, this option will warn about
    /// videos that do not reach that target. For example, 75 if file size is
    /// expected to be reduced by 25%. This option does not affect encoding.
    #[clap(long, value_parser = clap::value_parser!(u8).range(1..100))]
    expected_size: Option<u8>,
}

impl Args {
    pub(crate) fn get_video_codec(&self) -> Codec {
        if self.copy_streams {
            Codec::Copy
        } else if self.reference || self.for_tv {
            Codec::H264
        } else if !self.x265 && (self.av1 || !self.anime) {
            Codec::Av1
        } else {
            Codec::H265
        }
    }

    /// How many jobs should run in parallel?
    fn get_jobs(&self) -> Result<usize> {
        let jobs = match self.jobs {
            Some(0) => {
                bail!("Cannot run with 0 jobs.");
            }
            None => {
                if self.x265 {
                    max(1, (num_cpus::get_physical() as f64 / 3f64).round() as usize)
                } else {
                    max(1, num_cpus::get_physical() / 2)
                }
            }
            Some(n) => n,
        };

        Ok(jobs)
    }

    fn get_extra_flags(&self) -> Result<Vec<String>> {
        let whitespace_re = Regex::new(r"\s+")?;
        Ok(self
            .extra_flag
            .iter()
            .flat_map(|extra_flag| {
                // Split once on space, or leave as is if there's no space:
                whitespace_re.splitn(extra_flag, 2).map(String::from)
            })
            .collect())
    }

    pub(crate) fn get_extra_normal_flags(&self) -> Result<Vec<String>> {
        let raw = self.get_extra_flags()?;
        // Combine these to detect -vf and the arg together:
        // [-vf     hflip           ]
        // [        -vf        hflip]
        Ok(raw
            .iter()
            .chain(["".to_string()].iter())
            .zip(["".to_string()].iter().chain(raw.iter()))
            .filter_map(|(arg, prev_arg)| {
                if arg != "" && arg != "-vf" && prev_arg != "-vf" {
                    Some(arg.to_owned())
                } else {
                    None
                }
            })
            .collect())
    }

    pub(crate) fn get_extra_vf_flags(&self) -> Result<Vec<String>> {
        let raw = self.get_extra_flags()?;
        // Combine these to detect -vf and the arg together:
        // [-vf     hflip           ]
        // [        -vf        hflip]
        Ok(raw
            .iter()
            .chain(["".to_string()].iter())
            .zip(["".to_string()].iter().chain(raw.iter()))
            .filter_map(|(arg, prev_arg)| {
                if prev_arg == "-vf" {
                    Some(arg.to_owned())
                } else {
                    None
                }
            })
            .collect())
    }

    pub(crate) fn get_height(&self) -> u32 {
        if self.height_720p {
            720
        } else {
            1080
        }
    }
}

// Easily package different kinds of args as OsString
macro_rules! os_args {
    (str: $str:expr) => {
        $str.split_whitespace().map(|x| OsString::from(x)).collect::<Vec<_>>()
    };
    ($($x : expr),* $(,)?) => {
        vec![$(OsString::from($x)), *]
    };
}

enum Executable {
    FFMPEG,
    FFPROBE,
}

pub struct Encoder {
    args: Arc<Args>,
    exclude_as_paths: Vec<PathBuf>,
    include_as_paths: Vec<PathBuf>,
    ffmpeg_path: OsString,
    video_root: PathBuf,
}

impl Encoder {
    pub fn new(args: Args) -> Result<Encoder> {
        let exclude_as_paths = args.exclude.iter().map(PathBuf::from).collect();
        let include_as_paths = args.include.iter().map(PathBuf::from).collect();
        return Ok(Encoder {
            video_root: args.video_root.clone(),
            args: Arc::new(args),
            exclude_as_paths,
            include_as_paths,
            ffmpeg_path: find_executable(Executable::FFMPEG)?,
        });
    }

    pub async fn encode_videos(&self) -> Result<()> {
        let (failure_tx, failures) = channel();
        let input_files = self.get_video_paths().await?;
        let mut tasks_not_started = input_files
            .iter()
            .map(|input_file| self.encode_video(input_file, failure_tx.clone()))
            .collect::<VecDeque<_>>();

        let mut tasks_started = FuturesUnordered::new();
        for _ in 0..self.args.get_jobs().expect("Jobs should be set already") {
            if let Some(task) = tasks_not_started.pop_front() {
                log::trace!("Pushing a task into the job list (not started)");
                tasks_started.push(task);
            }
        }

        log::trace!("Will start jobs (concurrently)");
        while let Some(finished_task) = tasks_started.next().await {
            log::trace!("Popped a finished a task into the job list (not started)");
            finished_task?;
            if let Some(next_task) = tasks_not_started.pop_front() {
                log::trace!("Pushing another job to be run concurrently");
                tasks_started.push(next_task);
            } else {
                log::trace!("There are no more jobs to be started");
            }
        }
        log::trace!("Done with concurrent jobs");

        let failures: Vec<_> = failures.try_iter().collect();
        if failures.len() > 0 {
            log::warn!("Failure and warning summary:");
            for (path, msg) in failures {
                log::warn!("{}: {}", path.to_string_lossy(), msg);
            }
        }

        Ok(())
    }

    /// Multiple failure messages may be sent along the tx.
    async fn encode_video(&self, input: &InputFile, failure_tx: Sender<(PathBuf, String)>) -> Result<()> {
        let output_fname = input.get_output_path()?;
        let parent = output_fname
            .parent()
            .expect("Generated path must have a parent directory");
        if !parent.is_dir() {
            if parent.exists() {
                bail!("Cannot encode file to {output_fname:?} because the parent exists but is not a directory.");
            }
            // No need for a mutex, this is thread-safe:
            std::fs::create_dir_all(parent)?;
        }

        // Normal args for ffmpeg:
        let mut child_args = os_args!["-i", &input.path];
        // Options for -vf:
        let mut vf = Vec::<OsString>::new();

        child_args.extend(os_args!(
            str: "-nostdin -map_metadata 0 -movflags +faststart -movflags +use_metadata_tags -strict experimental"));
        let codec = self.args.get_video_codec();
        if codec != Codec::Copy {
            child_args.extend(os_args!["-crf", input.crf.to_string()]);
        }

        if !self.args.no_map_0 {
            child_args.extend(os_args!(str: "-map 0"));
        }

        if self.args.for_tv {
            if input.contains_subtitle().await? {
                if let Err(err) = add_subtitles(input, &mut vf).await
                {
                    failure_tx.send((input.path.to_owned(), format!("Error adding subtitles: {err:?}")))?;
                }

                // And don't include the existing soft subs:
                child_args.push("-sn".into());
            } else if let Some(sub_path) = find_subtitle_file(input)? {
                let sub_path = sub_path
                    .to_str()
                    .context("Could not convert subtitle name to utf-8.")?
                    .to_owned();
                let sub_path = escape_vf_path(&sub_path);
                let mut subs_option = OsString::from("subtitles=");
                subs_option.push(sub_path?);
                vf.push(subs_option);
            } else {
                child_args.extend(os_args!(str: "-c copy"));
            }
        } else {
            child_args.extend(os_args!(str: "-c copy"));
        }

        if let Some(audio_args) = self.get_audio_args(input).await {
            child_args.extend(audio_args);
        }

        let mut x265_params = self.get_x265_params(input.crf);
        if let Some(x265_params) = x265_params.as_mut() {
            let x265_params = x265_params.join(", ");
            child_args.extend(os_args!["-x265-params", &x265_params]);
        }

        if self.args.overwrite {
            child_args.extend(os_args!["-y"]);
        } else if output_fname.exists() {
            failure_tx.send((output_fname.to_owned(), format!("Output path already exists: {output_fname:?}")))?;
            return Ok(());
        }

        // Add the codec-specific flags:
        if codec == Codec::Copy {
            child_args.extend(os_args!(str: "-c:v copy"));
        } else {
            child_args.extend(match codec {
                Codec::Av1 => os_args!(str: "-c:v libaom-av1 -cpu-used"),
                Codec::H265 => os_args!(str: "-c:v libx265 -preset"),
                // NOTE: not tested. Let me know if these parameters don't work well with Chromecast,
                // or some other TV-related use-case.
                Codec::H264 if self.args.for_tv =>
                    os_args!(str: "-c:v libx264 -maxrate 10M -bufsize 16M -profile:v high -level 4.1 -preset"),
                Codec::H264 => os_args!(str: "-c:v libx264 -profile:v high -level 4.1 -preset"),
                _ => bail!("Codec not handled: {:?}", codec),
            });
            child_args.push(OsString::from(&self.args.preset));

            let max_height = self.args.get_height();
            // This -vf argument string was pretty thoroughly tested: it makes the shorter dimension equivalent to
            // the desired height (or width for portrait mode), without changing the aspect ratio, and without upscaling.
            // Using -2 instead of -1 ensures that the scaled dimension will be a factor of 2. Some filters need that.
            let vf_height = format!("scale=if(gte(iw\\,ih)\\,-2\\,min({max_height}\\,iw)):if(gte(iw\\,ih)\\,min({max_height}\\,ih)\\,-2)").into();
            let vf_pix_fmt: OsString = if self.args.eight_bit {
                "format=yuv420p".into()
            } else {
                "format=yuv420p10le".into()
            };
            vf.extend([vf_height, vf_pix_fmt]);
            vf.extend(self.args.get_extra_vf_flags()?.iter().map(|s| s.into()));

            // Transform list into string:
            let vf = {
                match &mut *vf {
                    [head, tail @ ..] => {
                        let builder = head;
                        for option in tail {
                            builder.push(", ");
                            builder.push(option);
                        }
                        builder
                    }
                    _ => bail!("vf cannot be empty"),
                }
            };

            // Add extra -vf arguments if they are set for this video:
            // foo.mp4 can have vf args set as VF_foo_mp4 or VF_foo
            if let Some(env_vf_args) = input.env_vf_args()? {
                _debug!(
                    input,
                    "Adding extra -vf arguments because environment variable was set"
                );
                vf.push(", ");
                vf.push(env_vf_args);
            }
            child_args.extend(os_args!["-vf", &vf]);
        }

        // Add other args specific to this filename
        if let Some(env_ffmpeg_args) = input.env_ffmpeg_args()? {
            child_args.extend(env_ffmpeg_args.split_whitespace().map(OsString::from));
        }

        child_args.extend(self.args.get_extra_normal_flags()?.iter().map(|s| s.into()));
        match env::var("FFMPEG_FLAGS") {
            Ok(env_args) => {
                child_args.extend(env_args.to_string().split_whitespace().map(|s| s.into()));
            }
            Err(env::VarError::NotPresent) => {}
            Err(err) => {
                _warn!(input, "Could not get extra ffmpeg args from FFMPEG_FLAGS: {err}");
            }
        }

        child_args.extend(os_args![&output_fname]);

        _info!(input, "");
        _info!(input, "Executing: {:?} {:?}", &self.ffmpeg_path, child_args);
        _info!(input, "");

        let mut program = Command::new(&self.ffmpeg_path);
        let mut command = program.args(child_args);
        if let Some(ref log_path) = input.log_path {
            let mut ffreport = OsString::from("file=");
            // ':' and '\' must be escaped:
            let lossy_logpath = log_path.to_string_lossy();
            if lossy_logpath.contains(':') || lossy_logpath.contains(r"\") {
                let lossy_logpath = lossy_logpath.replace(r"\", r"\\");
                let lossy_logpath = lossy_logpath.replace(":", r"\:");
                ffreport.push(lossy_logpath);
            } else {
                // It's preferable to not use lossy decoding unless characters need to be replaced:
                ffreport.push(log_path);
            }
            command = command.env("FFREPORT", ffreport);
        }
        let mut child = command
            .stdout(std::process::Stdio::piped())
            // Don't send stderr to a pipe because it makes ffmpeg buffer the output.
            // .stderr(process::Stdio::piped())
            .spawn()?;

        // Store the input file size in advance, because the user can delete the input file on Unix filesystems
        // after the encode begins. But don't return now because this is notfatal:
        let orig_size = get_file_size(&input.path);

        let mut child_stdout = child.stdout.take().unwrap();
        let mut child_stdout = Pin::new(&mut child_stdout);
        // let mut stderr = Box::new(child.stderr.take().unwrap()) as Box<dyn Read>;

        let mut buf = vec![0; 1024];
        loop {
            let exit_status = child.try_wait()?;
            let read_fut = child_stdout.read(&mut buf);
            select! {
                bytes_read = read_fut => {
                    std::io::stdout().lock().write_all(&buf[..bytes_read?])?;
                }
                else => {
                    // Nothing ready for read, so don't take up CPU time polling again right away
                    sleep(Duration::from_millis(50)).await;
                }
            };

            if let Some(exit_status) = exit_status {
                if !exit_status.success() {
                    let mut msg = format!("Error encoding {:?}. Check ffmpeg args", input.path);
                    if !self.args.no_map_0 {
                        msg = msg + ", or try again without `-map 0`";
                    }
                    // This error is significant enough to show right away, not just at the end:
                    _warn!(input, "{msg}");
                    failure_tx.send((input.path.to_owned(), msg)).unwrap();
                }

                if let Some(expected_size) = self.args.expected_size {
                    let size = match get_file_size(&output_fname) {
                        Ok(size) => size,
                        Err(err) => {
                            failure_tx.send((input.path.to_owned(), format!("Could not get file size after encoding: {err:?}")))?;
                            break;
                        },
                    };
                    let orig_size = match orig_size {
                        Ok(orig_size) => orig_size,
                        Err(err) => {
                            failure_tx.send((input.path.to_owned(), format!("Could not get original file disk space before encoding: {err:?}")))?;
                            break;
                        },
                    };
                    let percent = size * 100 / orig_size;
                    if percent > expected_size.into() {
                        failure_tx.send((input.path.to_owned(), format!("Output file was larger than expected at {percent}%: {output_fname:?}"))).unwrap();
                    } else if percent < (expected_size / 3).into() {
                        failure_tx.send((input.path.to_owned(), format!("Output file was much smaller than expected at {percent}%: {output_fname:?}"))).unwrap();
                    }
                }
                break;
            }
        }

        Ok(())
    }

    async fn get_audio_args(&self, input: &InputFile) -> Option<Vec<OsString>> {
        let default = Some(os_args!["-c:a", "aac", "-b:a", "128k", "-ac", "2"]);
        let audio_copy_arg = Some(os_args!["-c:a", "copy"]);
        if self.args.no_audio {
            _debug!(input, "Removing audio entirely, due to argument");
            return Some(os_args!["-an"]);
        } else if self.args.copy_audio {
            _debug!(
                input,
                "Skipping audio bitrate check and not encoding, due to argument"
            );
            return audio_copy_arg;
        } else if self.args.skip_audio_bitrate_check {
            _debug!(input, "Skipping audio bitrate check due to option chosen.");
            return default;
        } else if self.args.for_tv {
            _debug!(
                input,
                "Skipping audio bitrate check: always encode for TV playback"
            );
            return Some(os_args!["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
        }
        match input.get_audio_bitrate().await {
            Ok(bitrate) if bitrate <= 200f32 => {
                _debug!(input, "Audio bitrate is {bitrate} kb/s. Will not reencode");
                return audio_copy_arg;
            }
            Ok(bitrate) => {
                _trace!(input, "Audio bitrate is {bitrate} kb/s. Will reencode");
            }
            Err(err) => _warn!(input, "Could not get audio bitrate: {err}"),
        }
        return default;
    }

    fn get_x265_params(&self, crf: u8) -> Option<Vec<&str>> {
        if self.args.av1 || !self.args.anime {
            None
        } else {
            assert!(self.args.anime);

            // These encoding tips are from: https://kokomins.wordpress.com/2019/10/10/anime-encoding-guide-for-x265-and-why-to-never-use-flac/
            let x265_params = if self.args.anime_slow_well_lit {
                vec![
                    "bframes=8",
                    "psy-rd=1",
                    "aq-mode=3",
                    "aq-strength=0.8",
                    "deblock=1,1",
                ]
            } else if self.args.anime_mixed_dark_battle {
                if crf >= 19 {
                    // Note: recommended if: non-complex, motion only alternative
                    vec![
                        "bframes=8",
                        "psy-rd=1",
                        "psy-rdoq=1",
                        "aq-mode=3",
                        "qcomp=0.8",
                    ]
                } else {
                    // Note: recommended if: motion + fancy & detailed FX
                    vec![
                        "limit-sao",
                        "bframes=8",
                        "psy-rd=1.5",
                        "psy-rdoq=2",
                        "aq-mode=3",
                    ]
                }
            } else if crf > 19 {
                vec!["bframes=8", "psy-rd=1", "aq-mode=3"]
            } else {
                vec!["limit-sao", "bframes=8", "psy-rd=1", "aq-mode=3"]
            };
            Some(x265_params)
        }
    }

    /// Get the paths of all videos in the parent directory, excluding those in this directory.
    /// (This directory is considered the encode directory.)
    async fn get_video_paths(&self) -> Result<Vec<InputFile>> {
        let mut exclude = GlobSetBuilder::new();
        for pattern in &self.args.exclude {
            exclude.add(Glob::new(&pattern)?);
        }
        let exclude = exclude.build()?;

        let include = if self.args.include.is_empty() {
            None
        } else {
            let mut include = GlobSetBuilder::new();
            for pattern in &self.args.include {
                include.add(Glob::new(&pattern)?);
            }
            Some(include.build()?)
        };

        let video_re = Regex::new(
            r"^mp4|mkv|m4v|vob|ogg|ogv|wmv|yuv|y4v|mpg|mpeg|3gp|3g2|f4v|f4p|avi|webm|flv$",
        )?;
        let mut videos = Vec::new();
        let mut dirs = VecDeque::from([self.video_root.to_owned()]);
        let encode_dir = self.video_root.join(ENCODE_DIR);
        while let Some(dir) = dirs.pop_front() {
            let mut entries = Vec::new();
            for entry in dir.read_dir()? {
                entries.push(entry?);
            }
            entries.sort_by(|s1, s2| {
                lexical_sort::natural_lexical_cmp(&s1.path().to_string_lossy(), &s2.path().to_string_lossy())
            });
            for entry in entries {
                if let Some(limit) = self.args.limit {
                    if videos.len() == limit {
                        log::debug!("Reached video limit={limit}, won't encode any more");
                        return Ok(videos);
                    }
                }
                let fname = entry.path();
                let relative_path = pathdiff::diff_paths(fname.clone(), self.video_root.clone());
                let matchable_path = relative_path.unwrap_or(fname.clone());

                if let Some(include) = &include {
                    if !include.is_match(&matchable_path) {
                        if self.include_as_paths.iter().any(|incl| filename_is_match(incl, &matchable_path)) {
                            log::warn!("Path did not match an include pattern, but did match exactly. Including: {fname:?}");
                        } else {
                            log::debug!("Skipping path because it's not an included path: {fname:?}");
                            continue;
                        }
                    }
                }

                if fname == encode_dir {
                    continue;
                } else if exclude.is_match(&matchable_path) {
                    log::debug!("Skipping path because of exclude: {fname:?}");
                    continue;
                } else if self.exclude_as_paths.iter().any(|incl| filename_is_match(incl, &matchable_path)) {
                    log::warn!("Path did not match an exclude as a pattern, but did match exactly. Excluding: {fname:?}");
                    continue;
                }

                let md = entry.metadata()?;
                if md.is_dir() {
                    dirs.push_back(fname);
                } else if extension_matches(&fname, &video_re)? {
                    videos.push(InputFile::new(&fname, self.args.clone()).await?);
                }
            }
        }

        Ok(videos)
    }
}

async fn add_subtitles(input: &InputFile, vf_opts: &mut Vec<OsString>) -> Result<()> {
    let sub_file = tempfile::Builder::new()
        .suffix(".ass")
        .tempfile()?;
    let sub_path = sub_file.path();
    let escaped_sub_path = escape_vf_path(
        sub_path
        .to_str()
        .context("Could not convert temp path to utf-8. Needed for subtitles.")?,
    )?;
    dump_stream(&input.path, sub_path, false).await?;
    vf_opts.push(OsString::from(format!("subtitles={escaped_sub_path}")));
    Ok(())
}

fn get_file_size(output_fname: &PathBuf) -> Result<u64> {
    let md = output_fname.metadata()?;
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        return Ok(md.size());
    }
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        return Ok(md.file_size());
    }
}

fn extension_matches(fname: &PathBuf, video_re: &Regex) -> Result<bool> {
    if let Some(extension) = fname.extension() {
        let extension = extension.to_ascii_lowercase().to_str()
            .map(|s| s.to_string())
            .ok_or(anyhow!("Path can't be represented as utf-8: {:?}", &fname))?;
        return Ok(video_re.is_match(&extension));
    }
    return Ok(false);
}

fn filename_is_match(pattern: &PathBuf, matchable_path: &PathBuf) -> bool {
    if let Ok(canonical_pattern) = pattern.canonicalize() {
        if let Ok(canonical_path) = matchable_path.canonicalize() {
            if canonical_path == canonical_pattern {
                return true;
            }
        }
    }

    let pattern_comps = pattern.components();
    let path_comps = matchable_path.components();
    if pattern_comps.clone().count() > path_comps.clone().count() {
        return false;
    }

    return pattern_comps.rev().zip(path_comps.rev()).all(|(a, b)| a == b);
}

fn find_executable(executable: Executable) -> Result<OsString> {
    // let the user override the path to ffmpeg
    let (executable_name, environment_var) = match executable {
        Executable::FFMPEG => ("ffmpeg", "FFMPEG"),
        Executable::FFPROBE => ("ffprobe", "FFPROBE"),
    };

    if let Some(variable_value) = env::var_os(environment_var) {
        if !variable_value.is_empty() {
            return Ok(variable_value.into());
        }
    }
    Ok(executable_name.into())
}

#[allow(dead_code)]
/// Use ffmpeg to convert one path to another path, optionally with the `-c copy` option.
async fn dump_stream(input_path: &Path, output_path: &Path, copy: bool) -> Result<()> {
    let mut cmd = Command::new(find_executable(Executable::FFMPEG)?);
    let cmd = cmd.arg("-i").arg(input_path);
    let cmd = if copy { cmd.args(["-c", "copy"]) } else { cmd };
    let status = cmd.arg(output_path).status().await?;
    if !status.success() {
        warn!( "Could not convert path {input_path:?} to {output_path:?}");
    }
    Ok(())
}

/// Escape a path for use with the ffmpeg -vf argument. The escaping rules are hard to discover except
/// by testing.
fn escape_vf_path(sub_path: &str) -> Result<String> {
    let sub_path = sub_path.replace(r"\", r"\\\\");
    let sub_path = sub_path.replace(r"'", r"\\\'");
    let sub_path = sub_path.replace(r":", r"\\:");
    let sub_path = sub_path.replace(r"=", r"\\=");
    let sub_path = Regex::new(r"([;,\[\] =])")?.replace_all(&sub_path, "\\$1");
    Ok(sub_path.to_string())
}

fn find_subtitle_file(input: &InputFile) -> Result<Option<PathBuf>> {
    let srt_name = input
        .path
        .with_extension("srt")
        .file_name()
        .context("Could not get filename of video file")?
        .to_ascii_lowercase();
    for sibling in input
        .path
        .parent()
        .context("Could not get directory of video file")?
        .read_dir()?
    {
        if let Ok(sibling) = sibling {
            if sibling.file_name().to_ascii_lowercase() == srt_name {
                return Ok(Some(sibling.path()));
            }
        }
    }

    Ok(None)
}

#[test]
fn test_opt_codec() {
    let args = &Args::parse_from(["prog_name", "--av1"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name", "--x265"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--h265"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--anime"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--for-tv"]);
    assert_eq!(args.get_video_codec(), Codec::H264);

    let args = &Args::parse_from(["prog_name", "--reference"]);
    assert_eq!(args.get_video_codec(), Codec::H264);

    let args = &Args::parse_from(["prog_name", "--anime", "--aom-av1"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name", "--anime", "--aom-av1"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name", "--anime-mixed-dark-battle"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--anime-slow-well-lit"]);
    assert_eq!(args.get_video_codec(), Codec::H265);
}

#[test]
fn test_incompatible_opts() {
    assert!(matches!(
        Args::try_parse_from(["prog_name", "--anime-slow-well-lit", "--av1"]),
        Err(_)
    ));

    assert!(matches!(
        Args::try_parse_from(["prog_name", "--anime-mixed-dark-battle", "--av1"]),
        Err(_)
    ));
}

#[test]
fn test_crf() {
    let args = Args::parse_from("prog_name --include '**/*Online*Course*' $USERPROFILE/dwhelper/ --overwrite --no-audio --x265 --no-log --crf 26".split_whitespace());
    assert_eq!(args.crf, Some(26));
}

#[test]
fn test_output_fname() {
    use tokio::runtime::Runtime;

    let args = Arc::new(Args::parse_from(["prog_name", "--av1", "a/b"]));

    let rt = Runtime::new().unwrap();
    let input = rt
        .block_on(InputFile::new(Path::new("a/b/vid.en.MP4"), args))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("a/b/encoded/vid.en-5-crf24.mp4")
    );
    assert_eq!(
        input.log_path,
        Some(PathBuf::from("a/b/encoded/vid.en.MP4.log"))
    );

    let args = Arc::new(Args::parse_from(["prog_name", "--x265", "--no-log", "a/b"]));
    let input = rt
        .block_on(InputFile::new(
            Path::new("a/b/subdir/vid.mp4"),
            args.clone(),
        ))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("a/b/encoded/subdir/vid-crf22.mp4")
    );
    assert_eq!(input.log_path, None);

    let input = rt
        .block_on(InputFile::new(Path::new("a/b/vid.MKV"), args.clone()))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("a/b/encoded/vid-crf22.mkv")
    );

    let input = rt
        .block_on(InputFile::new(
            Path::new("outside-root/vid.mkv"),
            args.clone(),
        ))
        .unwrap();
    assert!(matches!(input.get_output_path(), Err(_)));

    let args = Arc::new(Args::parse_from(["prog_name", "--x265", "--no-log", "/a"]));
    let input = rt
        .block_on(InputFile::new(Path::new("/a/vid.flv"), args.clone()))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("/a/encoded/vid-crf22.mkv")
    );

    let args = Arc::new(Args::parse_from([
        "prog_name",
        "--copy-streams",
        "--no-log",
        "/a",
    ]));
    let input = rt
        .block_on(InputFile::new(Path::new("/a/vid.flv"), args.clone()))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("/a/encoded/vid-crf0.mkv")
    );

    let args = Arc::new(Args::parse_from([
        "prog_name",
        "--reference",
        "--no-log",
        "/a",
    ]));
    let input = rt
        .block_on(InputFile::new(Path::new("/a/vid.flv"), args.clone()))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("/a/encoded/vid-crf8.mkv")
    );
}

#[test]
fn test_preset() -> Result<()> {
    let args = &Args::parse_from(["prog_name"]);
    assert_eq!(args.x265, false);
    assert_eq!(args.eight_bit, false);
    assert_eq!(args.preset, "5");
    let args = &Args::parse_from(["prog_name", "--preset=3"]);
    assert_eq!(args.preset, "3");
    let args = &Args::parse_from(["prog_name", "--for-tv"]);
    assert_eq!(args.preset, "fast");
    assert_eq!(args.eight_bit, true);
    let args = &Args::parse_from(["prog_name", "--x265"]);
    assert_eq!(args.preset, "slow");

    Ok(())
}

#[test]
fn test_extra_flags() -> Result<()> {
    let args = &Args::parse_from([
        "prog_name",
        "--extra-flag",
        "-vf hflip",
        "--extra-flag",
        "-ss 30",
        "--extra-flag=-vf bwdif",
        "--extra-flag=-t 5:00",
    ]);

    let correct_extra_flags = ["-ss", "30", "-t", "5:00"]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let correct_extra_vf_flags = ["hflip", "bwdif"]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    assert_eq!(args.get_extra_normal_flags()?, correct_extra_flags);
    assert_eq!(args.get_extra_vf_flags()?, correct_extra_vf_flags);

    let args = &Args::parse_from([
        "prog_name",
        "--extra-flag",
        "-ss 30",
        "--extra-flag",
        "-vf hflip",
        "--extra-flag=-t 5:00",
        "--extra-flag=-vf bwdif",
    ]);

    assert_eq!(args.get_extra_normal_flags()?, correct_extra_flags);
    assert_eq!(args.get_extra_vf_flags()?, correct_extra_vf_flags);

    Ok(())
}
