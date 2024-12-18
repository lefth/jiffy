use std::{
    cmp::{max, min},
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
use crate::{find_executable, get_output_dir, normalize_path, Cli, Codec, Executable};

pub struct InputFile {
    pub path: PathBuf,
    pub log_path: Option<PathBuf>,
    pub crf: u8,
    cli: Arc<Cli>,
}

impl InputFile {
    pub async fn new(path: &Path, cli: Arc<Cli>) -> Result<Self> {
        let mut ret = Self {
            path: path.to_owned(),
            log_path: Self::get_log_path(path, &cli)?,
            crf: u8::MAX, // placeholder
            cli,
        };
        ret.init().await?;
        Ok(ret)
    }

    /// Trim off the front part of an input path, so the video root
    /// directory is not included.
    ///
    /// If the video root is:    a/b/videos
    /// And the video path is:   a/b/videos/2009/june/x.mp4
    /// The result path will be:            2009/june/x.mp4
    fn trim_input_path(input_path: &Path, video_root: &Path) -> Result<PathBuf> {
        let input_path = normalize_path(input_path);
        let video_root = normalize_path(video_root);

        if video_root == PathBuf::from(".") {
            assert!(
                input_path.is_relative(),
                "Video rooted in path '.' must be relative."
            );
            return Ok(input_path.to_owned());
        }

        if !input_path.starts_with(&video_root) {
            bail!("Videos should be in the video root.");
        }
        Ok(input_path
            .components()
            .skip(video_root.components().count())
            .collect::<PathBuf>())
    }

    pub fn fill_output_template(
        naming_format: &str,
        directory: PathBuf,
        basename: &str,
        preset: &str,
        crf: &str,
        extension: &str,
    ) -> PathBuf {
        let name = naming_format.replace("{basename}", basename);
        let name = name.replace("{preset}", preset);
        let name = name.replace("{crf}", crf);
        // Don't use with_extension() since we must add it, not change it:
        let name = format!("{name}.{extension}");
        directory.join(PathBuf::from(name))
    }

    pub fn get_output_path(&self, naming_format: Option<String>) -> Result<PathBuf> {
        let output_dir = get_output_dir(&self.cli);

        let extension = self
            .path
            .extension()
            .map(|extension| extension.to_ascii_lowercase().to_string_lossy().to_string());
        // Let mp4 keep its extension, but change others to mkv:
        let extension = match extension.as_deref() {
            Some("mp4") => "mp4",
            _ => "mkv",
        };

        let basename = Self::trim_input_path(&self.path, &self.cli.video_root)?
            .with_extension("")
            .to_string_lossy()
            .to_string();

        let naming_format = naming_format.unwrap_or({
            if self.cli.get_video_codec() == Codec::Av1 {
                "{basename}-{preset}-crf{crf}"
            } else {
                "{basename}-crf{crf}"
            }
            .into()
        });

        Ok(Self::fill_output_template(
            &naming_format,
            output_dir,
            &basename,
            &self.cli.preset,
            &self.crf.to_string(),
            extension,
        ))
    }

    /// Get the log path for this input file. Also create the directory for the log
    /// file, since logging starts before encoding, so the directory may not exist
    /// if we delay.
    fn get_log_path(input_path: &Path, cli: &Cli) -> Result<Option<PathBuf>> {
        if cli.test_opts.no_log {
            Ok(None)
        } else {
            let output_dir = get_output_dir(cli);
            let mut output = output_dir
                .join(Self::trim_input_path(&input_path, &cli.video_root)?)
                .into_os_string();
            output.push(".log");

            Ok(Some(output.into()))
        }
    }

    async fn init(&mut self) -> Result<()> {
        let codec = self.cli.get_video_codec();
        self.crf = if let Some(crf) = self.cli.crf {
            crf
        } else {
            let mut crf = match codec {
                Codec::Av1 => 24,
                Codec::H265 => 22,
                Codec::H264 if self.cli.for_tv => 17,
                Codec::H264 => 8, // if not for TV, this old codec is most useful for making a reference clip
                Codec::Copy => 0,
            };
            if self.cli.anime {
                crf += 3;
            }

            match self.get_video_dimensions().await {
                Ok((w, h)) if codec != Codec::Copy => {
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
                _ => {}
            }

            crf
        };

        Ok(())
    }

    /// Returns bitrate in kb/second, for example 128 or 256.
    pub(crate) async fn get_audio_bitrate(&self) -> Result<f32> {
        let seconds = self.get_audio_seconds().await?;
        Ok(self.get_audio_size_kb().await? / seconds * 8f32)
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
        let captures = time_regex.captures_iter(&output).last().expect(&format!(
            "Could not find a time in the ffmpeg output. A bug report containing the input file \
                or the ffmpeg output would be appreciated. Input file={}",
            &self.path.to_string_lossy()
        ));

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
    async fn get_audio_size_kb(&self) -> Result<f32> {
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

    pub async fn contains_subtitle(&self) -> Result<bool> {
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

        let str = self.path.to_string_lossy();
        let output = String::from_utf8(output.stdout)?;
        // Some versions of ffprobe add an extra 'x' at the end:
        let re = Regex::new(r"(\d+)x(\d+)x?").unwrap();
        return re
            .captures(&output)
            .map(|cap| {
                (
                    cap.get(1).unwrap().as_str().parse::<u32>().unwrap(),
                    cap.get(2).unwrap().as_str().parse::<u32>().unwrap(),
                )
            })
            .context(format!(
                "Could not parse dimensions of file '{str}': '{output}'"
            ));
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

    pub(crate) fn create_log_directory(&self) -> Result<()> {
        if let Some(log_path) = self.log_path.as_ref() {
            let parent = log_path
                .parent()
                .context(format!("Log path does not have a parent: {log_path:?}"))?;
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}
