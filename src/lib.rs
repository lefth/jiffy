use std::{
    cmp::max,
    collections::VecDeque,
    env,
    ffi::{OsStr, OsString},
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        mpsc::{channel, Sender},
        Arc,
    },
    time::Duration,
    u128,
};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use futures::stream::{FuturesUnordered, StreamExt};
use globset::{Glob, GlobSetBuilder};
use log::warn;
use rand::Rng;
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
}

#[derive(Parser)]
pub struct Args {
    /// Set the quality level (for either encoded). The default is 24 for AV1
    /// and 22 for H265, but if unspecified, a better CRF may be used for small
    /// videos, or a lower quality CRF may be used for animation.
    #[clap(long)]
    crf: Option<u8>,

    /// Use x265 instead of aom-av1. This is true by default with --animation.
    #[clap(long, alias = "h265", conflicts_with = "av1")]
    x265: bool,

    /// Use libaom-av1 for encoding. This is the default, except for animation.
    #[clap(long = "av1", aliases = &["aom", "libaom", "aom-av1"])]
    av1: bool,

    /// Use settings that work well for anime or animation.
    #[clap(long = "animation", alias = "anime",
        default_value_ifs = &[
            ("anime-slow-well-lit", None, Some("true")),
            ("anime-mixed-dark-battle", None, Some("true")),
        ],
        min_values(0),
    )]
    anime: bool,

    /// Use this setting for slow well lit anime, like slice of life:
    #[clap(long, conflicts_with_all = &["av1", "anime-mixed-dark-battle"])]
    anime_slow_well_lit: bool,

    /// Use this setting for anime with some dark scenes, some battle scenes (shonen, historical, etc.)
    #[clap(long, conflicts_with_all = &["av1", "anime-slow-well-lit"])]
    anime_mixed_dark_battle: bool,

    /// Encode this many videos in parallel. The default varies per encoder.
    #[clap(long, short, alias = "max-jobs")]
    jobs: Option<usize>,

    /// Encode as 720p. Otherwise the video will be 1080p. The source size is taken into consideration;
    /// in no case is a video scaled up.
    #[clap(long = "720p")]
    height_720p: bool,

    /// Encode as 8-bit. Otherwise the video will be 10-bit.
    #[clap(
        long = "8-bit",
        alias = "8bit",
        default_value_if("for-tv", None, Some("true"))
    )]
    pub eight_bit: bool,

    /// The encoding preset to use--by default this is fairly slow. By default, "6" for libaom, "slow" for x265.
    #[clap(
        long,
        default_value = "6",
        hide_default_value = true,
        default_value_if("x265", None, Some("slow")),
        default_value_if("for-tv", None, Some("fast"))
    )]
    pub preset: String,

    /// Overwrite existing output files
    #[clap(long)]
    pub overwrite: bool,

    /// Add additional ffmpeg flags, such as "-to 5:00" to quickly test the first few minutes of a file.
    /// Each option should be passed separately, for example: `jiffy --extra-flag='-ss 30' --extra-flag='-t 5:00'`
    #[clap(long, allow_hyphen_values(true))]
    pub extra_flag: Vec<String>,

    /// Don't write log files for each ffmpeg invocation. This avoids polluting your output directory
    /// with a log file per input.
    #[clap(long, short)]
    pub no_log: bool,

    /// Don't check if the audio streams are within acceptable limits--just reencode them. This saves
    /// a little time in some circumstances.
    #[clap(long = "skip-bitrate-check")]
    pub skip_audio_bitrate_check: bool,

    /// Encode the videos in this directory. By default, encode in the current directory.
    /// Output files are put in "video_root/encoded". If the given path ends in "encoded",
    /// the real video root is taken to be the parent directory.
    #[clap(default_value = ".", hide_default_value = true)]
    pub video_root: PathBuf,

    /// Paths (usually glob patterns) that can be excluded. They match from the video encode root.
    /// For example, "*S01*/*E01*" might be used to skip the first episode of a TV show, and "**/*E01*" would
    /// skip the first episode of each season. This argument must be given once per exclude pattern.
    #[clap(long)]
    pub exclude: Vec<String>,

    /// Run ffmpeg without `-map 0`. This occasionally fixes an encoding error.
    #[clap(long, default_value_if("for-tv", None, Some("true")))]
    pub no_map_0: bool,

    /// Encode a certain number of files, then stop.
    #[clap(long)]
    pub limit: Option<usize>,

    /// Make a high quality but inefficient file for low spec televisions. The output is intended for
    /// watching, not for archival purposes. This is the only option that encodes with x264.
    /// Subtitles are hard-coded if available. These files should be compatible with Chromecast without
    /// the need for transcoding.
    // If you find this option is not compatible with your TV, please let me know what model and what encoding
    // options do work.
    #[clap(long, conflicts_with_all = &["av1", "x265", "anime", "anime-slow-well-lit", "anime-mixed-dark-battle"], takes_value(false))]
    pub for_tv: bool,
}

impl Args {
    pub(crate) fn get_codec(&self) -> Codec {
        if self.for_tv {
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
                    max(1, num_cpus::get_physical() / 3)
                } else {
                    max(1, num_cpus::get_physical() / 2)
                }
            }
            Some(n) => n,
        };

        Ok(jobs)
    }

    pub(crate) fn get_extra_flags(&self) -> Result<Vec<String>> {
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

/// Get the paths of all videos in the parent directory, excluding those in this directory.
/// (This directory is considered the encode directory.)
pub(crate) async fn get_video_paths(video_root: &Path, args: &Arc<Args>) -> Result<Vec<InputFile>> {
    let mut exclude = GlobSetBuilder::new();
    for pattern in &args.exclude {
        exclude.add(Glob::new(&pattern)?);
    }
    let exclude = exclude.build()?;

    let video_re =
        Regex::new(r"^mp4|mkv|m4v|vob|ogg|ogv|wmv|yuv|y4v|mpg|mpeg|3gp|3g2|f4v|f4p|avi|webm|flv$")?;
    let mut videos = Vec::new();
    let mut dirs = VecDeque::from([video_root.to_owned()]);
    let encode_dir = video_root.join(ENCODE_DIR);
    while let Some(dir) = dirs.pop_front() {
        let mut entries = Vec::new();
        for entry in dir.read_dir()? {
            entries.push(entry?);
        }
        entries.sort_by(|s1, s2| {
            human_sort::compare(&s1.path().to_string_lossy(), &s2.path().to_string_lossy())
        });
        for entry in entries {
            if let Some(limit) = args.limit {
                if videos.len() == limit {
                    log::debug!("Reached video limit={}, won't encode any more", limit);
                    return Ok(videos);
                }
            }
            let fname = entry.path();
            if exclude.is_match(&fname) {
                log::debug!("Skipping path because of exclude: {:?}", fname);
                continue;
            }
            if fname == encode_dir {
                continue;
            }
            let md = entry.metadata()?;
            if md.is_dir() {
                dirs.push_back(fname);
            } else {
                let extension = fname
                    .extension()
                    .map(|ext| -> Result<_> {
                        ext.to_ascii_lowercase()
                            .to_str()
                            .map(|s| s.to_string())
                            .ok_or(anyhow!("Path can't be represented as utf-8: {:?}", &fname))
                    })
                    .transpose()?;

                match extension {
                    Some(extension) if video_re.is_match(&extension) => {
                        videos.push(InputFile::new(&fname, args.clone()).await?)
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(videos)
}

pub async fn encode_videos(args: &Arc<Args>) -> Result<()> {
    let ffmpeg_path = find_executable(Executable::FFMPEG)?;

    let (failure_tx, failures) = channel();
    let input_files = get_video_paths(&args.video_root, args).await?;
    let mut tasks_not_started = input_files
        .iter()
        .map(|input_file| encode_video(input_file, &ffmpeg_path, args, failure_tx.clone()))
        .collect::<VecDeque<_>>();

    let mut tasks_started = FuturesUnordered::new();
    for _ in 0..args.get_jobs().expect("Jobs should be set already") {
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
        log::warn!("Failure summary:");
        for failed_path in failures {
            log::warn!("Failed to encode: {:?}", &failed_path);
        }
    }

    Ok(())
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

async fn encode_video(
    input: &InputFile,
    ffmpeg: &OsStr,
    args: &Args,
    failure_tx: Sender<PathBuf>,
) -> Result<()> {
    let output_fname = input.get_output_path()?;
    let parent = output_fname
        .parent()
        .expect("Generated path must have a parent directory");
    if !parent.is_dir() {
        if parent.exists() {
            bail!(
                "Cannot encode file to {:?} because the parent exists but is not a directory.",
                &output_fname
            );
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
    child_args.extend(os_args!["-crf", input.crf.to_string()]);

    if !args.no_map_0 {
        child_args.extend(os_args!(str: "-map 0"));
    }

    if args.for_tv {
        if input.get_has_subtitles().await? {
            // TODO: move the subtitles to a temp file so the name doesn't need to be escaped:
            // (Don't literally move them; use dump_stream to temp.ass)

            let sub_path = escape_vf_path(
                input
                    .path
                    .to_str()
                    .context("Could not convert video path to utf-8. Needed for subtitles.")?,
            );
            let mut subs_option = OsString::from("subtitles=");
            subs_option.push(sub_path?);
            vf.push(subs_option);

            // And don't include the existing soft subs:
            child_args.push("-sn".into());
        } else if let Some(sub_path) = try_find_subs(input)? {
            // NOTE: in my testing, converting .srt to .ass gives a smaller rendered text that looks better:
            let sub_path = convert_ass(&sub_path).await?;

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

    if let Some(audio_args) = get_audio_args(input, args).await {
        child_args.extend(audio_args);
    }

    let mut x265_params = get_x265_params(args, input.crf);
    if let Some(x265_params) = x265_params.as_mut() {
        let x265_params = x265_params.join(", ");
        child_args.extend(os_args!["-x265-params", &x265_params]);
    }

    if args.overwrite {
        child_args.extend(os_args!["-y"]);
    }

    // Add the codec-specific flags:
    child_args.extend(match args.get_codec() {
        Codec::Av1 => os_args!(str: "-c:v libaom-av1 -cpu-used"),
        Codec::H265 => os_args!(str: "-c:v libx265 -preset"),
        // Source for parameters that work well with chromecast:
        Codec::H264 => {
            // NOTE: not tested. Let me know if these parameters don't work well with Chromecast,
            // or some other TV-related use-case.
            os_args!(str: "-c:v libx264 -maxrate 10M -bufsize 16M -profile:v high -level 4.1 -preset")
        }
    });
    child_args.push(OsString::from(&args.preset));

    child_args.extend(args.get_extra_flags()?.iter().map(|s| s.into()));
    match env::var("FFMPEG_FLAGS") {
        Ok(env_args) => {
            child_args.extend(env_args.to_string().split_whitespace().map(|s| s.into()));
        }
        Err(env::VarError::NotPresent) => {}
        Err(err) => {
            _warn!(
                input,
                "Could not get extra ffmpeg args from FFMPEG_FLAGS: {}",
                err,
            );
        }
    }

    let max_height = args.get_height();
    // This -vf argument string was pretty thoroughly tested: it makes the shorter dimension equivalent to
    // the desired height (or width for portrait mode), without changing the aspect ratio, and without upscaling.
    // Using -2 instead of -1 ensures that the scaled dimension will be a factor of 2. Some filters need that.
    let vf_height = format!(
        "scale=if(gte(iw\\,ih)\\,-2\\,min({}\\,iw)):if(gte(iw\\,ih)\\,min({}\\,ih)\\,-2)",
        max_height, max_height
    )
    .into();
    let vf_pix_fmt: OsString = if args.eight_bit {
        "format=yuv420p".into()
    } else {
        "format=yuv420p10le".into()
    };
    vf.extend([vf_height, vf_pix_fmt]);

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

    // Add other args specific to this filename
    if let Some(env_ffmpeg_args) = input.env_ffmpeg_args()? {
        child_args.extend(env_ffmpeg_args.split_whitespace().map(OsString::from));
    }

    child_args.extend(os_args![&output_fname]);

    _info!(input, "");
    _info!(input, "Executing: {:?} {:?}", ffmpeg, child_args);
    _info!(input, "");

    let mut program = Command::new(ffmpeg);
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
                _warn!(
                    input,
                    "Error encoding {:?}. Check ffmpeg args{}",
                    input.path,
                    if args.no_map_0 {
                        ""
                    } else {
                        ", or try again without `-map 0`"
                    }
                );
                failure_tx.send(input.path.to_owned()).unwrap();
            }
            break;
        }
    }

    Ok(())
}

/// Convert SRT to ASS (though it would actually work to convert any video file with a subtitle stream to ASS.)
async fn convert_ass(sub_path: &PathBuf) -> Result<PathBuf> {
    let converted_sub_path =
        std::env::temp_dir().join(format!("tmp-sub-{}.ass", rand::thread_rng().gen::<u128>()));
    dump_stream(&sub_path, &converted_sub_path, false).await?;
    Ok(converted_sub_path)
}

/// Use ffmpeg to convert one path to another path, optionally with the `-c copy` option.
async fn dump_stream(input_path: &Path, output_path: &Path, copy: bool) -> Result<()> {
    let mut cmd = Command::new(find_executable(Executable::FFMPEG)?);
    let cmd = cmd.arg("-i").arg(input_path);
    let cmd = if copy { cmd.args(["-c", "copy"]) } else { cmd };
    let status = cmd.arg(output_path).status().await?;
    if !status.success() {
        warn!(
            "Could not convert path {:?} to {:?}",
            input_path, output_path
        )
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

fn try_find_subs(input: &InputFile) -> Result<Option<PathBuf>> {
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

async fn get_audio_args(input: &InputFile, args: &Args) -> Option<Vec<OsString>> {
    let default = Some(os_args!["-c:a", "aac", "-b:a", "128k", "-ac", "2"]);
    if args.skip_audio_bitrate_check {
        _debug!(input, "Skipping audio bitrate check due to option chosen.");
        return default;
    } else if args.for_tv {
        _debug!(
            input,
            "Skipping audio bitrate check: always encode for TV playback"
        );
        return Some(os_args!["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
    }
    match input.get_audio_bitrate().await {
        Ok(bitrate) if bitrate <= 200f32 => {
            _debug!(
                input,
                "Audio bitrate is {} kb/s. Will not reencode",
                bitrate
            );
            return Some(os_args!["-c:a", "copy"]);
        }
        Ok(bitrate) => {
            _trace!(input, "Audio bitrate is {} kb/s. Will reencode", bitrate);
        }
        Err(err) => _warn!(input, "Could not get audio bitrate: {}", err),
    }
    return default;
}

fn get_x265_params(args: &Args, crf: u8) -> Option<Vec<&str>> {
    if args.av1 || !args.anime {
        None
    } else {
        assert!(args.anime);

        // These encoding tips are from: https://kokomins.wordpress.com/2019/10/10/anime-encoding-guide-for-x265-and-why-to-never-use-flac/
        let x265_params = if args.anime_slow_well_lit {
            vec![
                "bframes=8",
                "psy-rd=1",
                "aq-mode=3",
                "aq-strength=0.8",
                "deblock=1,1",
            ]
        } else if args.anime_mixed_dark_battle {
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

#[test]
fn test_opt_codec() {
    let args = &Args::parse_from(["prog_name", "--av1"]);
    assert_eq!(args.get_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name"]);
    assert_eq!(args.get_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name", "--x265"]);
    assert_eq!(args.get_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--h265"]);
    assert_eq!(args.get_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--anime"]);
    assert_eq!(args.get_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--for-tv"]);
    assert_eq!(args.get_codec(), Codec::H264);

    let args = &Args::parse_from(["prog_name", "--anime", "--aom-av1"]);
    assert_eq!(args.get_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name", "--anime", "--aom-av1"]);
    assert_eq!(args.get_codec(), Codec::Av1);

    let args = &Args::parse_from(["prog_name", "--anime-mixed-dark-battle"]);
    assert_eq!(args.get_codec(), Codec::H265);

    let args = &Args::parse_from(["prog_name", "--anime-slow-well-lit"]);
    assert_eq!(args.get_codec(), Codec::H265);
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
fn test_output_fname() {
    use tokio::runtime::Runtime;

    let args = Arc::new(Args::parse_from(["prog_name", "--av1", "a/b"]));

    let rt = Runtime::new().unwrap();
    let input = rt
        .block_on(InputFile::new(Path::new("a/b/vid.en.MP4"), args))
        .unwrap();
    assert_eq!(
        input.get_output_path().unwrap(),
        PathBuf::from("a/b/encoded/vid.en-6-crf24.mp4")
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
}

#[test]
fn test_preset() -> Result<()> {
    let args = &Args::parse_from(["prog_name"]);
    assert_eq!(args.eight_bit, false);
    assert_eq!(args.preset, "6");
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
        "-ss 30",
        "--extra-flag=-t 5:00",
    ]);
    let extra_flags = args.get_extra_flags()?;
    assert_eq!(
        extra_flags,
        ["-ss", "30", "-t", "5:00"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );

    Ok(())
}
