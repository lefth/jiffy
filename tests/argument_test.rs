use anyhow::*;
use clap::Parser;
use jiffy::*;

#[test]
fn test_opt_codec() {
    let args = &Cli::parse_from(["prog_name", "--av1"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Cli::parse_from(["prog_name"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Cli::parse_from(["prog_name", "--x265"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Cli::parse_from(["prog_name", "--h265"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Cli::parse_from(["prog_name", "--anime"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Cli::parse_from(["prog_name", "--for-tv"]);
    assert_eq!(args.get_video_codec(), Codec::H264);

    let args = &Cli::parse_from(["prog_name", "--reference"]);
    assert_eq!(args.get_video_codec(), Codec::H264);

    let args = &Cli::parse_from(["prog_name", "--anime", "--aom-av1"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Cli::parse_from(["prog_name", "--anime", "--aom-av1"]);
    assert_eq!(args.get_video_codec(), Codec::Av1);

    let args = &Cli::parse_from(["prog_name", "--anime-mixed-dark-battle"]);
    assert_eq!(args.get_video_codec(), Codec::H265);

    let args = &Cli::parse_from(["prog_name", "--anime-slow-well-lit"]);
    assert_eq!(args.get_video_codec(), Codec::H265);
}

#[test]
fn test_incompatible_opts() {
    assert!(matches!(
        Cli::try_parse_from(["prog_name", "--anime-slow-well-lit", "--av1"]),
        Err(_)
    ));

    assert!(matches!(
        Cli::try_parse_from(["prog_name", "--anime-mixed-dark-battle", "--av1"]),
        Err(_)
    ));
}

#[test]
fn test_crf() {
    let args = Cli::parse_from("prog_name --include '**/*Online*Course*' $USERPROFILE/dwhelper/ --overwrite --no-audio --x265 --no-log --crf 26".split_whitespace());
    assert_eq!(args.crf, Some(26));
}

#[test]
fn test_preset() -> Result<()> {
    let args = &Cli::parse_from(["prog_name"]);
    assert_eq!(args.x265, false);
    assert_eq!(args.eight_bit, false);
    assert_eq!(args.preset, "slow");
    let args = &Cli::parse_from(["prog_name", "--preset=3"]);
    assert_eq!(args.preset, "3");
    let args = &Cli::parse_from(["prog_name", "--for-tv"]);
    assert_eq!(args.preset, "fast");
    assert_eq!(args.eight_bit, true);
    let args = &Cli::parse_from(["prog_name", "--av1"]);
    assert_eq!(args.preset, "5");
    let args = &Cli::parse_from(["prog_name", "--x265"]);
    assert_eq!(args.preset, "slow");

    Ok(())
}

#[test]
fn test_extra_flags() -> Result<()> {
    let args = &Cli::parse_from([
        "prog_name",
        "--extra-flag",
        "-vf hflip",
        "--extra-flag",
        "-ss 30",
        "--extra-flag=-vf bwdif",
        "--extra-flag=-t 5:00",
    ]);

    let args2 = &Cli::parse_from([
        "prog_name",
        "--extra-flag",
        "-ss 30",
        "--extra-flag",
        "-vf hflip",
        "--extra-flag=-t 5:00",
        "--extra-flag=-vf bwdif",
    ]);

    let expected_extra_flags = ["-ss", "30", "-t", "5:00"].into_iter().collect::<Vec<_>>();
    let expected_extra_vf_flags = ["hflip", "bwdif"].into_iter().collect::<Vec<_>>();

    assert_eq!(args.get_extra_normal_flags()?, expected_extra_flags);
    assert_eq!(args.get_extra_vf_flags()?, expected_extra_vf_flags);
    assert_eq!(args2.get_extra_normal_flags()?, expected_extra_flags);
    assert_eq!(args2.get_extra_vf_flags()?, expected_extra_vf_flags);

    Ok(())
}
