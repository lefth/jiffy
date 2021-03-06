# jiffy

<!-- [![Build Status](https://travis-ci.com/lefth/jiffy.svg?branch=master)](https://travis-ci.com/lefth/jiffy) -->

A wrapper for ffmpeg that runs multiple jobs at the same time. This program is meant to handle many types
of videos while containing the most commonly used options. The main encoders used are AOMedia AV1 and x265.

```
USAGE:
    jiffy [OPTIONS] [--] [VIDEO_ROOT]

ARGS:
    <VIDEO_ROOT>    Encode the videos in this directory. By default, encode in the current
                    directory. Output files are put in "video_root/encoded". If the given path
                    ends in "encoded", the real video root is taken to be the current directory

OPTIONS:
        --720p                       Encode as 720p. Otherwise the video will be 1080p. The source
                                     size is taken into consideration; in no case is a video scaled
                                     up
        --8-bit <eight-bit>          Encode as 8-bit. Otherwise the video will be 10-bit
        --animation <anime>...       Use settings that work well for anime or animation
        --anime-mixed-dark-battle    Use this setting for anime with some dark scenes, some battle
                                     scenes (shonen, historical, etc.)
        --anime-slow-well-lit        Use this setting for slow well lit anime, like slice of life:
        --av1                        Use libaom-av1 for encoding. This is the default, except for
                                     animation
        --crf <CRF>                  Set the quality level (for either encoded). The default is 24
                                     for AV1 and 22 for H265, but if unspecified, a better CRF may
                                     be used for small videos, or a lower quality CRF may be used
                                     for animation
        --exclude <EXCLUDE>          Paths (usually glob patterns) that can be excluded. They match
                                     from the video encode root. For example, "*S01*/*E01*" might be
                                     used to skip the first episode of a TV show, and "**/*E01*"
                                     would skip the first episode of each season. This argument must
                                     be given once per exclude pattern
        --extra-flag <EXTRA_FLAG>    Add additional ffmpeg flags, such as "-to 5:00" to quickly test
                                     the first few minutes of a file. Each option should be passed
                                     separately, for example: `jiffy --extra-flag='-ss 30'
                                     --extra-flag='-t 5:00'`
        --for-tv                     Make a high quality but inefficient file for low spec
                                     televisions. The output is intended for watching, not for
                                     archival purposes. This is the only option that encodes with
                                     x264. Subtitles are hard-coded if available
    -h, --help                       Print help information
    -j, --jobs <JOBS>                Encode this many videos in parallel. The default varies per
                                     encoder
        --limit <LIMIT>              Encode a certain number of files, then stop
    -n, --no-log                     Don't write log files for each ffmpeg invocation. This avoids
                                     polluting your output directory with a log file per input
        --no-map-0                   Run ffmpeg without `-map 0`. This occasionally fixes an
                                     encoding error
        --overwrite                  Overwrite existing output files
        --preset <PRESET>            The encoding preset to use--by default this is fairly slow. By
                                     default, "6" for libaom, "slow" for x265
        --skip                       Don't check if the audio streams are within acceptable
                                     limits--just reencode them. This saves a little time in some
                                     circumstances
        --x265                       Use x265 instead of aom-av1. This is true by default with
                                     --animation
```

### Environment variables

Extra flags for ffmpeg can also be passed in the `FFMPEG_FLAGS` environment variable.
Per-video flags can be set as well, so settings per video will be remembered. (It is
best to store these files in a wrapper for `jiffy` so as to not pollute your
shell init with video configurations.)

The video-specific environment variables can be `FFMPEG_video_name` and `VF_video_name` for general ffmpeg
flags and `-vf` flags respectively.
Note that non-alphanumeric characters and leading numbers should be replaced by "_". Examples:

```
FFMPEG_video_20200104_mp4="--enable-dnl-denoising=0 --denoise-noise-level=8"
FFMPEG_video_20220101_mp4="-ss 15 -to 2:30" VF_video_20220101_mp4="eq=brightness=0.06:saturation=2"
VF_video_20200104_mp4=hflip,vflip
```

## Installation

After Rust is installed, run:

`cargo install --git https://github.com/lefth/jiffy`

## Quirks

The output filename is currently hard coded to: `input-filename-crf<CRF>.mp4`
(or `.mkv`), or `input-filename-<PROFILE>-crf<CRF>.mp4` for AV1 videos. This
helps distinguish between encodes and originals, but it's not beautiful.

It would be ideal to configure the output filename based on a template in a
config file or environment variable formatted like the following:
`{INPUT_FILENAME_STEM}-{PROFILE}.{EXTENSION}`. Patches are welcome.

Symlinks within the encode directory are not handled intelligently. The same
video can end up being encoded multiple times. Ideally symlinks should be
dereferenced if they point outside the video directory, and transformed
if they point within the video directory.

## See also

https://github.com/Alkl58/NotEnoughAV1Encodes
https://github.com/master-of-zen/Av1an
