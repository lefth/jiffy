use std::{
    cmp::max,
    collections::VecDeque,
    env,
    ffi::OsString,
    fs::remove_file,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        mpsc::{channel, Sender},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgAction, Parser};
use futures::stream::{FuturesUnordered, StreamExt};
use globset::{Glob, GlobSetBuilder};
use lexical_sort;
#[allow(unused_imports)]
use log::*;
use regex::Regex;

pub mod input_file;
pub use input_file::*;
pub mod logger;
#[allow(unused_imports)]
pub use logger::*;
use tokio::{io::AsyncReadExt, process::Command, select, time::sleep};

pub const ENCODED: &str = "encoded";

#[derive(PartialEq, std::fmt::Debug)]
pub enum Codec {
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
    pub crf: Option<u8>,

    /// Use x265 instead of aom-av1. This is true by default with --animation.
    #[clap(long, alias = "h265", conflicts_with_all = ["av1", "reference"])]
    pub x265: bool,

    /// Use x264 to make a high quality (high disk space) fast encode.
    #[clap(long, conflicts_with_all = ["av1", "x265"])]
    pub reference: bool,

    /// Use libaom-av1 for encoding. This is the default, except for animation.
    #[clap(long = "av1", aliases = ["aom", "libaom", "aom-av1"])]
    pub av1: bool,

    /// Use settings that work well for anime or animation.
    #[clap(long = "animation", alias = "anime",
        default_value_ifs = [
            ("anime_slow_well_lit", "true", "true"),
            ("anime_mixed_dark_battle", "true", "true"),
        ],
    )]
    pub anime: bool,

    /// Use this setting for slow well lit anime, like slice of life:
    #[clap(long, conflicts_with_all = ["av1", "anime_mixed_dark_battle", "reference"])]
    pub anime_slow_well_lit: bool,

    /// Use this setting for anime with some dark scenes, some battle scenes (shonen, historical, etc.)
    #[clap(long, conflicts_with_all = ["av1", "anime_slow_well_lit", "reference"])]
    pub anime_mixed_dark_battle: bool,

    /// Encode this many videos in parallel. The default varies per encoder.
    #[clap(long, short, alias = "max-jobs")]
    pub jobs: Option<usize>,

    /// Encode as 720p. Otherwise the video will be 1080p. The source size is taken into
    /// consideration; in no case is a video scaled up.
    #[clap(long = "720p")]
    pub height_720p: bool,

    /// Encode as 8-bit.  Otherwise the video will be 10-bit, except if creating
    /// a file as reference or for TV. However, this depends on the compilation
    /// options of the encoder.
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

    /// Can specify -q -q (-qq) to make the program ever more quiet.
    #[clap(long, short, action = ArgAction::Count)]
    pub quiet: u8,

    /// Increase the log verbosity.
    #[clap(long, short, action = ArgAction::Count)]
    pub verbose: u8,

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
    pub expected_size: Option<u8>,

    /// If an output file is larger than expected (or larger than the original),
    /// it will be deleted. This prevents accidentally re-encoding highly
    /// compressed videos to lower compression, losing quality in the process.
    #[clap(long, requires("expected_size"))]
    pub delete_too_large: bool,

    /// Files smaller than this size will be skipped. If there is no suffix,
    /// it's taken to mean megabytes.
    #[clap(long)]
    pub minimum_size: Option<String>,

    /// Output files will be written with this name. Fields that will be filled:
    /// {preset}, {basename}, {crf}
    /// For example: --output-name "{basename}-crf{crf}"
    #[clap(long, aliases = ["output-format", "name-format", "naming-format"])]
    pub output_name: Option<String>,

    /// Output files will be saved in this directory. By default, it is
    /// <VIDEO_ROOT>/encoded.
    #[clap(long, short, aliases = ["output-directory", "output-dir", "output-path"])]
    pub output_dir: Option<PathBuf>,
}

impl Args {
    pub fn get_video_codec(&self) -> Codec {
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

    pub fn get_extra_normal_flags(&self) -> Result<Vec<String>> {
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

    pub fn get_extra_vf_flags(&self) -> Result<Vec<String>> {
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

struct EncodingErr(PathBuf, String);

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
            if let Err(EncodingErr(path, msg)) = finished_task {
                failure_tx.send((path, msg))?;
            }
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

    async fn encode_video(
        &self,
        input: &InputFile,
        failure_tx: Sender<(PathBuf, String)>,
    ) -> Result<(), EncodingErr> {
        let input_path = input.path.clone();
        if let Err(err) = self.encode_video_inner(input, failure_tx).await {
            return Err(EncodingErr(input_path, format!("{err:?}")));
        }
        Ok(())
    }

    /// Multiple failure messages may be sent along the tx.
    async fn encode_video_inner(
        &self,
        input: &InputFile,
        failure_tx: Sender<(PathBuf, String)>,
    ) -> Result<()> {
        let output_path = input.get_output_path(self.args.output_name.clone())?;
        let parent = output_path
            .parent()
            .expect("Generated path must have a parent directory");
        if !parent.is_dir() {
            if parent.exists() {
                bail!("Cannot encode file to {output_path:?} because the parent exists but is not a directory.");
            }
            // No need for a mutex, this is thread-safe:
            std::fs::create_dir_all(parent)?;
        }

        // Normal args for ffmpeg:
        let mut child_args = os_args!["-i", &input.path, "-hide_banner"];
        // Options for -vf:
        let mut vf = Vec::<OsString>::new();

        match self.args.quiet as i8 - self.args.verbose as i8 {
            ..0 => {}
            0 => {
                child_args.extend(os_args!(str: "-loglevel info"));
            }
            1 => {
                child_args.extend(os_args!(str: "-loglevel warning"));
            }
            2 => {
                child_args.extend(os_args!(str: "-loglevel error"));
                child_args.extend(os_args!(str: "-x265-params loglevel=warning"));
                // child_args.extend(os_args!(str: "-aom-params quiet")); // not supported
            }
            3.. => {
                child_args.extend(os_args!(str: "-loglevel error"));
                child_args.extend(os_args!(str: "-x265-params loglevel=error"));
                // child_args.extend(os_args!(str: "-aom-params quiet")); // not supported
            }
        }

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
                if let Err(err) = add_subtitles(input, &mut vf).await {
                    failure_tx.send((
                        input.path.to_owned(),
                        format!("Error adding subtitles: {err:?}"),
                    ))?;
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
        } else if output_path.exists() {
            failure_tx.send((
                output_path.to_owned(),
                format!("Output path already exists: {output_path:?}"),
            ))?;
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
                _ => bail!("Codec not handled: {codec:?}"),
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
                _warn!(
                    input,
                    "Could not get extra ffmpeg args from FFMPEG_FLAGS: {err}"
                );
            }
        }

        child_args.extend(os_args![&output_path]);

        _info!(input, "");
        _info!(input, "Executing: {:?} {:?}", &self.ffmpeg_path, child_args);
        _info!(input, "");

        let mut program = Command::new(&self.ffmpeg_path);
        let mut command = program.args(child_args);
        if let Some(ref log_path) = input.log_path {
            input.create_log_directory()?;
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
        let orig_size = get_file_size(&input.path).context(format!(
            "Could not get original file disk space before encoding"
        ))?;
        if input_too_small(orig_size, &self.args.minimum_size)? {
            bail!("Skipping file as too small to encode");
        }

        let mut child = command
            .stdout(std::process::Stdio::piped())
            // Don't send stderr to a pipe because it makes ffmpeg buffer the output.
            // .stderr(process::Stdio::piped())
            .spawn()?;

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
                    let mut msg = String::from("Encoding error. Check ffmpeg args");
                    if !self.args.no_map_0 {
                        msg += ", or try again without `-map 0`";
                    }
                    // This error is significant enough to show right away, not just at the end:
                    _warn!(input, "{:?}: {}", input.path, msg);
                    failure_tx.send((input.path.to_owned(), msg)).unwrap();
                }

                self.check_encoded_size(orig_size, input.path.clone(), output_path, failure_tx)?;
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
        let encode_dir = get_output_dir(&self.args);
        while let Some(dir) = dirs.pop_front() {
            let mut entries = Vec::new();
            for entry in dir.read_dir()? {
                entries.push(entry?);
            }
            entries.sort_by(|s1, s2| {
                lexical_sort::natural_lexical_cmp(
                    &s1.path().to_string_lossy(),
                    &s2.path().to_string_lossy(),
                )
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
                        if self
                            .include_as_paths
                            .iter()
                            .any(|incl| is_same_file(incl, &matchable_path))
                        {
                            log::warn!("Path did not match an include pattern, but did match exactly. Including: {fname:?}");
                        } else {
                            log::debug!(
                                "Skipping path because it's not an included path: {fname:?}"
                            );
                            continue;
                        }
                    }
                }

                if is_same_file(&fname, &encode_dir) {
                    continue;
                } else if exclude.is_match(&matchable_path) {
                    log::debug!("Skipping path because of exclude: {fname:?}");
                    continue;
                } else if self
                    .exclude_as_paths
                    .iter()
                    .any(|incl| is_same_file(incl, &matchable_path))
                {
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

    fn check_encoded_size(
        &self,
        orig_size: u64,
        input_path: PathBuf,
        output_path: PathBuf,
        failure_tx: Sender<(PathBuf, String)>,
    ) -> Result<()> {
        let size = get_file_size(&output_path).context("Could not get file size after encoding")?;
        if size < 300 {
            failure_tx
                .send((
                    input_path,
                    format!("Deleting {size} byte output file: {output_path:?}"),
                ))
                .unwrap();
            remove_file(output_path)?;
            return Ok(());
        }

        let percent = size * 100 / orig_size;
        if let Some(expected_size) = self.args.expected_size {
            if percent > expected_size.into() {
                if self.args.delete_too_large {
                    failure_tx.send((input_path, format!("Deleting too large output file (too large at {percent}%): {output_path:?}"))).unwrap();
                    remove_file(output_path)?;
                } else {
                    failure_tx.send((input_path, format!("Output file was larger than expected at {percent}%: {output_path:?}"))).unwrap();
                }
            } else if percent < (expected_size / 3).into() {
                failure_tx.send((input_path, format!("Output file was much smaller than expected at {percent}%: {output_path:?}"))).unwrap();
            } else if percent > 100 && self.args.delete_too_large {
                failure_tx.send((input_path, format!("Deleting output file larger than the original ({percent}%): {output_path:?}"))).unwrap();
                remove_file(output_path)?;
            }
        }

        return Ok(());
    }
}

pub fn parse_size(input: &str) -> Result<u64> {
    let msg = "Size string must be a number with optional K, M, G suffix";
    let input = input.to_lowercase();
    let captures = Regex::new(r"^(\.\d+|\d+(?:\.\d*)?)([bkmgt])?$")?
        .captures(&input)
        .context(msg)?;
    let n = captures
        .get(1)
        .context(msg)?
        .as_str()
        .parse::<f64>()
        .context(msg)?;
    let suffix = captures.get(2).map_or("m", |m| m.as_str());
    let factor = match suffix {
        "b" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1u64 << 40,
        _ => bail!(msg),
    };
    Ok((factor as f64 * n) as u64)
}

pub fn input_too_small(size: u64, input_str: &Option<String>) -> Result<bool> {
    if let Some(input_str) = input_str {
        let input = parse_size(input_str)?;
        return Ok(size < input);
    }
    Ok(false)
}

async fn add_subtitles(input: &InputFile, vf_opts: &mut Vec<OsString>) -> Result<()> {
    let sub_file = tempfile::Builder::new().suffix(".ass").tempfile()?;
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

fn get_file_size(output_fname: &Path) -> Result<u64> {
    let md = output_fname.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(md.size());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return Ok(md.file_size());
    }
}

// From Cargo: https://github.com/rust-lang/cargo/blob/7b7af3077bff8d60b7f124189bc9de227d3063a9/crates/cargo-util/src/paths.rs#L84
/// Normalize a path, removing things like `.` and `..`.
///
/// CAUTION: This does not resolve symlinks
pub fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    ret
}

fn extension_matches(fname: &Path, video_re: &Regex) -> Result<bool> {
    if let Some(extension) = fname.extension() {
        let extension = extension
            .to_ascii_lowercase()
            .to_str()
            .map(|s| s.to_string())
            .ok_or(anyhow!("Path can't be represented as utf-8: {:?}", &fname))?;
        return Ok(video_re.is_match(&extension));
    }
    return Ok(false);
}

fn is_same_file(pattern: &Path, matchable_path: &Path) -> bool {
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

    return pattern_comps
        .rev()
        .zip(path_comps.rev())
        .all(|(a, b)| a == b);
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

/// Use ffmpeg to convert one path to another path, optionally with the `-c copy` option.
async fn dump_stream(input_path: &Path, output_path: &Path, copy: bool) -> Result<()> {
    let mut cmd = Command::new(find_executable(Executable::FFMPEG)?);
    let cmd = cmd.arg("-i").arg(input_path);
    let cmd = if copy { cmd.args(["-c", "copy"]) } else { cmd };
    let status = cmd.arg(output_path).status().await?;
    if !status.success() {
        warn!("Could not convert path {input_path:?} to {output_path:?}");
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

pub fn get_output_dir(args: &Args) -> PathBuf {
    args.output_dir
        .as_ref()
        .map_or(args.video_root.join(ENCODED), |path| path.to_owned())
}
