use clap::Parser;
use jiffy::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

macro_rules! assert_paths_eq {
    ($path:expr, $expected:expr) => {{
        let path = normalize_path(&$path);

        let expected = PathBuf::from($expected);
        let expected = normalize_path(&expected);

        assert_eq!(path, expected)
    }};
}

#[test]
fn test_fill_output_template() {
    assert_paths_eq!(
        InputFile::fill_output_template(
            "{basename}-{preset}-{crf}",
            PathBuf::from("dir"),
            "foo",
            "preset",
            "crf",
            "avi"
        ),
        "dir/foo-preset-crf.avi"
    );
    assert_paths_eq!(
        InputFile::fill_output_template(
            "{basename}",
            PathBuf::from("dir"),
            "foo",
            "preset",
            "crf",
            "avi"
        ),
        "dir/foo.avi"
    );
}

#[tokio::test]
async fn test_output_directory() {
    let args = Arc::new(Cli::parse_from([
        "prog_name",
        "--output-directory",
        "encoded",
    ]));
    let input = InputFile::new(Path::new("a/b/vid.en.MP4"), args)
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "encoded/a/b/vid.en-5-crf24.mp4"
    );
}

#[tokio::test]
async fn test_input_output_directories() {
    // All equivalent:
    let encode_directories = ["a/b", "./a/b"];
    let input_paths = ["a/b/vid.en.MP4", "./a/b/vid.en.MP4"];

    for encode_directory in encode_directories {
        for input_path in input_paths {
            let args = Arc::new(Cli::parse_from([
                "prog_name",
                "--output-directory",
                "encoded",
                encode_directory,
            ]));

            let input_path = Path::new(input_path);
            let input = InputFile::new(input_path, args).await.unwrap();
            assert_paths_eq!(
                input.get_output_path(None).unwrap(),
                "encoded/vid.en-5-crf24.mp4"
            );
        }
    }
}

#[tokio::test]
async fn test_absolute_output_directory() {
    let args = Arc::new(Cli::parse_from([
        "prog_name",
        "--output-directory",
        "/home/x/encoded",
    ]));

    let input = InputFile::new(Path::new("a/b/vid.en.MP4"), args)
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "/home/x/encoded/a/b/vid.en-5-crf24.mp4"
    );
}

#[tokio::test]
async fn test_input_dir_and_absolute_output_dir() {
    let args = Arc::new(Cli::parse_from([
        "prog_name",
        "--output-directory",
        "/home/x/encoded",
        "./a/b",
    ]));

    let input = InputFile::new(Path::new("a/b/vid.en.MP4"), args)
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "/home/x/encoded/vid.en-5-crf24.mp4"
    );
}

#[tokio::test]
async fn test_output_fname() {
    let args = Arc::new(Cli::parse_from(["prog_name", "--av1", "a/b"]));

    let input = InputFile::new(Path::new("a/b/vid.en.MP4"), args)
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "a/b/encoded/vid.en-5-crf24.mp4"
    );
    assert_paths_eq!(
        input
            .get_output_path(Some(String::from("{basename}-{preset}-crf{crf}")))
            .unwrap(),
        "a/b/encoded/vid.en-5-crf24.mp4"
    );
    assert_paths_eq!(
        input
            .get_output_path(Some(String::from("{basename}-crf{crf}")))
            .unwrap(),
        "a/b/encoded/vid.en-crf24.mp4"
    );
    assert_paths_eq!(input.log_path.unwrap(), "a/b/encoded/vid.en.MP4.log");

    let args = Arc::new(Cli::parse_from(["prog_name", "--x265", "--no-log", "a/b"]));
    let input = InputFile::new(Path::new("a/b/subdir/vid.mp4"), args.clone())
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "a/b/encoded/subdir/vid-crf22.mp4"
    );
    assert_eq!(input.log_path, None);

    let input = InputFile::new(Path::new("a/b/vid.MKV"), args.clone())
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "a/b/encoded/vid-crf22.mkv"
    );

    let input = InputFile::new(Path::new("outside-root/vid.mkv"), args.clone())
        .await
        .unwrap();
    assert!(matches!(input.get_output_path(None), Err(_)));

    let args = Arc::new(Cli::parse_from(["prog_name", "--x265", "--no-log", "/a"]));
    let input = InputFile::new(Path::new("/a/vid.flv"), args.clone())
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "/a/encoded/vid-crf22.mkv"
    );

    let args = Arc::new(Cli::parse_from([
        "prog_name",
        "--copy-streams",
        "--no-log",
        "/a",
    ]));
    let input = InputFile::new(Path::new("/a/vid.flv"), args.clone())
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "/a/encoded/vid-crf0.mkv"
    );

    let args = Arc::new(Cli::parse_from([
        "prog_name",
        "--reference",
        "--no-log",
        "/a",
    ]));
    let input = InputFile::new(Path::new("/a/vid.flv"), args.clone())
        .await
        .unwrap();
    assert_paths_eq!(
        input.get_output_path(None).unwrap(),
        "/a/encoded/vid-crf8.mkv"
    );
}

#[test]
#[should_panic(expected = "Could not build glob pattern")]
fn test_include_bad_glob() {
    Encoder::get_matcher_from_globs(".", &vec!["a -!｜：([]).mp4".to_string()], true);
}

#[test]
fn test_include_bad_glob_okay_if_exists() {
    let path = "b -!｜：([]).mp4".to_string();
    std::fs::File::create(&path).expect("Could not create test file");
    let matcher = Encoder::get_matcher_from_globs(".", &vec![path.clone()], true)
        .expect("Could not create matcher");

    // make sure it is matched:
    assert!(Encoder::is_match(&matcher, &path));
    std::fs::remove_file(&path).expect("Could not remove test file");
}
