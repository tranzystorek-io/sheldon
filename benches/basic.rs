use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

use criterion::{criterion_group, criterion_main, Criterion};

const PLUGINS: &[&str] = &[
    "zsh-users/zsh-autosuggestions",
    "wting/autojump",
    "zsh-users/zsh-syntax-highlighting",
    "StackExchange/blackbox",
    "sobolevn/git-secret",
    "b4b4r07/enhancd",
    "fcambus/ansiweather",
    "chriskempson/base16-shell",
    "supercrabtree/k",
    "zsh-users/zsh-history-substring-search",
    "wfxr/forgit",
    "zdharma/fast-syntax-highlighting",
    "iam4x/zsh-iterm-touchbar",
    "unixorn/git-extra-commands",
    "MichaelAquilina/zsh-you-should-use",
    "mfaerevaag/wd",
    "zsh-users/zaw",
    "Tarrasch/zsh-autoenv",
    "mafredri/zsh-async",
    "djui/alias-tips",
    "agkozak/zsh-z",
    "changyuheng/fz",
    "b4b4r07/emoji-cli",
    "Tarrasch/zsh-bd",
    "zdharma/history-search-multi-word",
];

/// Returns the path to the Sheldon binary.
fn bin() -> PathBuf {
    let bin_dir = env::var_os("CARGO_BIN_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env::current_exe().ok().map(|mut path| {
                path.pop();
                if path.ends_with("deps") {
                    path.pop();
                }
                path
            })
        })
        .unwrap();
    assert_eq!(bin_dir.file_name().unwrap(), "release");
    bin_dir.join("sheldon")
}

/// Run the given Sheldon subcommand with the given home directory.
fn run(home: &Path, subcmd: &str) {
    let process::Output {
        status,
        stdout,
        stderr,
    } = process::Command::new(bin())
        .env_clear()
        .env("HOME", home)
        .arg("--verbose")
        .arg(subcmd)
        .output()
        .unwrap();
    let stdout = String::from_utf8(stdout).unwrap();
    let stderr = String::from_utf8(stderr).unwrap();
    assert!(
        status.success(),
        format!("STDOUT:\n{}\nSTDERR:\n{}\n", stdout, stderr)
    );
}

pub fn bench(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    // setup
    let mut config = String::new();
    for plugin in PLUGINS {
        config.push_str("plugins.'");
        config.push_str(plugin);
        config.push_str("'.github = '");
        config.push_str(plugin);
        config.push_str("'\n");
    }
    let config_dir = home.join(".sheldon");
    fs::create_dir(&config_dir).unwrap();
    fs::write(config_dir.join("plugins.toml"), config).unwrap();
    run(home, "lock");

    // actually benchmark
    c.bench_function("load-25-plugins", |b| b.iter(|| run(home, "source")));
}

criterion_group!(benches, bench);
criterion_main!(benches);
