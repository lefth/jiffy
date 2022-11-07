use std::{
    cmp::min,
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use regex::Regex;
use tokio::process::Command;

#[allow(unused_imports)]
use crate::{_debug, _error, _info, _log, _trace, _warn};
use crate::{find_executable, Args, Codec, Executable, ENCODE_DIR};

pub struct InputFile {
    pub path: PathBuf,
    pub log_path: Option<PathBuf>,
    pub crf: u8,
    args: Arc<Args>,
}

impl InputFile {
    pub(crate) async fn new(path: &Path, args: Arc<Args>) -> Result<Self> {
        let mut ret = Self {
            path: path.to_owned(),
            log_path: Self::get_log_path(path, &args)?,
            crf: u8::MAX, // placeholder
            args,
        };
        ret.init().await;
        Ok(ret)
    }

    /// Trim off the front part of an input path, so the video root
    /// directory is not included.
    ///
    /// If the video root is:    a/b/videos
    /// And the video path is:   a/b/videos/2009/june/x.mp4
    /// The result path will be:            2009/june/x.mp4
    fn trim_input_path(input_path: &Path, video_root: &Path) -> Result<PathBuf> {
        if !input_path.starts_with(video_root) {
            bail!("Videos should be in the video root.");
        }
        Ok(input_path
            .components()
            .skip(video_root.components().count())
            .collect::<PathBuf>())
    }

    pub(crate) fn get_output_path(&self) -> Result<PathBuf> {
        let mut output = self
            .args
            .video_root
            .join(ENCODE_DIR)
            .join(Self::trim_input_path(&self.path, &self.args.video_root)?);

        let extension = output
            .extension()
            .map(|extension| extension.to_ascii_lowercase().to_string_lossy().to_string());
        let is_mp4 = matches!(extension, Some(extension) if extension == "mp4");
        // Let mp4 keep its extension, but change others to mkv:
        let extension = if is_mp4 { "mp4" } else { "mkv" };
        output.set_extension("");
        let mut output = output.into_os_string();
        if self.args.get_codec() == Codec::Av1 {
            output.push(format!("-{}", self.args.preset));
        }
        output.push(format!("-crf{}", self.crf));

        // don't call set_extension because we already removed it, and any
        // ".something" in the filename will be interpreted as an extension:
        // let mut output = PathBuf::from(output);
        //output.set_extension(extension);

        output.push(".");
        output.push(extension);
        let output = PathBuf::from(output);

        Ok(output)
    }

    /// Get the log path for this input file. Also create the directory for the log
    /// file, since logging starts before encoding, so the directory may not exist
    /// if we delay.
    fn get_log_path(input_path: &Path, args: &Args) -> Result<Option<PathBuf>> {
        if args.no_log {
            Ok(None)
        } else {
            let mut output = args
                .video_root
                .join(ENCODE_DIR)
                .join(Self::trim_input_path(&input_path, &args.video_root)?)
                .into_os_string();
            output.push(".log");

            let mut parent = PathBuf::from(&output);
            if !parent.pop() {
                bail!("Generated path must have a parent directory");
            }

            if !parent.is_dir() {
                if parent.exists() {
                    bail!(
                        "Cannot make log file {:?} because the parent exists but is not a directory.",
                        &output
                    );
                }
                // No need for a mutex, this is thread-safe:
                std::fs::create_dir_all(parent)?;
            }

            Ok(Some(output.into()))
        }
    }

    async fn init(&mut self) {
        self.crf = if let Some(crf) = self.args.crf {
            crf
        } else {
            let mut crf = match self.args.get_codec() {
                Codec::Av1 => 24,
                Codec::H265 => 22,
                Codec::H264 => 17,
            };
            if self.args.anime {
                crf += 3;
            }

            match self.get_video_dimensions().await {
                Ok((w, h)) => {
                    let max_dimension = max(w, h);
                    if max_dimension < 1920 {
                        // Smaller videos need better CRF, so subtract some points--
                        // But 1080p needs no delta, but let's give 720p or lower delta=4.
                        let shrinkage = 1920 - max_dimension;
                        let delta = min(
                            4,
                            (4f32 * shrinkage as f32 / (1920 - 1080) as f32).round() as u8,
                        );
                        if delta > 0 {
                            _info!(
                                &*self,
                                "Changing inferred CRF from {} to {} because input is small",
                                crf,
                                crf - delta
                            );
                            crf -= delta;
                        }
                    }
                }
                Err(err) => {
                    log::warn!(
                        "Error running ffprobe, could not get video dimensions: {}",
                        err
                    );
                }
            }

            crf
        };
    }

    /// Returns bitrate in kb/second, for example 128 or 256.
    pub(crate) async fn get_audio_bitrate(&self) -> Result<f32> {
        let seconds = self.get_audio_seconds().await?;
        Ok(self.get_audio_size().await? / seconds * 8f32)
    }

    async fn get_audio_seconds(&self) -> Result<f32> {
        let ffprobe = find_executable(Executable::FFPROBE)?;

        _trace!(self, "Trying to get the length from the container");
        let output = Command::new(&ffprobe)
            .args(
                "-v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1"
                    .split_whitespace(),
            )
            .arg(&self.path)
            .output()
            .await?
            .stdout;
        let output = String::from_utf8_lossy(&output).to_owned();
        // It's not an error for this to fail. The duration may not be specified in this way:
        if let Ok(seconds) = output.parse::<f32>() {
            return Ok(seconds);
        }

        _trace!(self, "Trying to get the length from the video stream");
        let output = Command::new(&ffprobe)
            .args(
                "-v error -select_streams v:0 -show_entries stream=duration -of default=noprint_wrappers=1:nokey=1"
                    .split_whitespace(),
            )
            .arg(&self.path)
            .output()
            .await?
            .stdout;
        let output = String::from_utf8_lossy(&output).to_owned();
        // It's not an error for this to fail. The duration may not be specified in this way:
        if let Ok(seconds) = output.parse::<f32>() {
            return Ok(seconds);
        }

        _trace!(self, "Trying to get the length by decoding it all");
        // This method is really slow:
        // Decode the file and look for "time=00:01:03.48"
        //
        // NOTE: the simpler commands that do this don't work on all files.
        // See: https://trac.ffmpeg.org/wiki/FFprobeTips
        let ffmpeg = find_executable(Executable::FFMPEG)?;
        let output = Command::new(&ffmpeg)
            .arg("-i")
            .arg(&self.path)
            .args("-vn -f null -".split_whitespace())
            .output()
            .await?
            .stderr;
        let output = String::from_utf8_lossy(&output).to_owned();

        let time_regex = Regex::new(r"(\d+):(\d{2}):(\d{2}\.\d+)").unwrap();
        let captures = time_regex.captures_iter(&output).last().expect(
            "Could not find a time in the ffmpeg output. A bug report containing the input file \
                or the ffmpeg output would be appreciated.",
        );

        let seconds = captures.get(1).unwrap().as_str().parse::<f32>()? * 3600f32
            + captures.get(2).unwrap().as_str().parse::<f32>()? * 60f32
            + captures.get(3).unwrap().as_str().parse::<f32>()?;
        _trace!(
            self,
            "Stream seconds: {} (parsed from \"{}\")",
            seconds,
            captures.get(0).unwrap().as_str()
        );

        Ok(seconds)
    }

    /// Get the audio stream's size in kilobytes
    async fn get_audio_size(&self) -> Result<f32> {
        _trace!(self, "Calculating audio size");
        let ffprobe = find_executable(Executable::FFPROBE)?;
        let output = Command::new(ffprobe)
            .args("-v error -select_streams a -show_entries packet=size -of default=nokey=1:noprint_wrappers=1".split_whitespace())
            .arg(&self.path)
            .output()
            .await?
            .stdout;
        let output = String::from_utf8_lossy(&output);
        let mut sum = 0f32;
        for line in output.lines() {
            if let Ok(bytes) = line.parse::<f32>() {
                sum += bytes;
            } else {
                _warn!(
                    self,
                    "Ignoring non-numeric line when getting the audio size: {}",
                    line,
                );
            }
        }
        sum /= 1024f32;
        _trace!(self, "Audio size: {}", sum);
        Ok(sum)
    }

    pub async fn get_has_subtitles(&self) -> Result<bool> {
        let ffmpeg = find_executable(Executable::FFMPEG)?;
        let status = Command::new(ffmpeg)
            .arg("-i")
            .arg(&self.path)
            .args("-c copy -map 0:s:0 -frames:s 1 -f null - -v 0 -hide_banner".split_whitespace())
            .status()
            .await?;
        // The command returns true only if a subtitle is in the video
        Ok(status.success())
    }

    async fn get_video_dimensions(&self) -> Result<(u32, u32)> {
        let ffprobe_path = find_executable(Executable::FFPROBE)?;
        let output = Command::new(&ffprobe_path)
            .args(
                "-v error -select_streams v:0 -show_entries stream=width,height -of csv=s=x:p=0"
                    .split_whitespace(),
            )
            .arg(&self.path)
            .output()
            .await?;

        let output = String::from_utf8(output.stdout)?;
        let (width, height) = scan_fmt::scan_fmt!(&output, "{}x{}", u32, u32)?;
        Ok((width, height))
    }

    /// Get the last part of the filename (without directory parts).
    pub fn basename(&self) -> Result<OsString> {
        self.path
            .components()
            .last()
            .map(|comp| comp.as_os_str().to_owned())
            .context("Path is empty")
    }

    /// Get the core part of the filename (without directory parts or extension).
    pub fn core_filename(&self) -> Result<OsString> {
        Ok(PathBuf::from(self.basename()?)
            .with_extension("")
            .into_os_string())
    }

    fn env_arg_names(&self) -> Result<[String; 2]> {
        Ok([self.basename()?, self.core_filename()?].map(|name| {
            let name = name.to_string_lossy();
            let name = Regex::new("[^a-zA-Z0-9_]").unwrap().replace_all(&name, "_");
            name.to_string()
        }))
    }

    /// Get the environment variables set for this video, in the format of VF_name.mp4,
    /// with the extension being optional and all special characters being replaced by '_'.
    pub(crate) fn env_vf_args(&self) -> Result<Option<OsString>> {
        let var_names = self.env_arg_names()?.map(|name| format!("VF_{}", name));
        Ok(env::var_os(&var_names[0]).or(env::var_os(&var_names[1])))
    }

    /// Get the environment variables set for this video, in the format of FFMPEG_name.mp4,
    /// with the extension being optional and all special characters being replaced by '_'.
    pub(crate) fn env_ffmpeg_args(&self) -> Result<Option<String>> {
        let var_names = self.env_arg_names()?.map(|name| format!("FFMPEG_{}", name));
        Ok(env::var_os(&var_names[0])
            .or(env::var_os(&var_names[1]))
            .map(|s| {
                s.to_str()
                    .map(|s| s.to_owned())
                    .context("Additional ffmpeg argument was not proper UTF-8")
            })
            .transpose()?)
    }
}
