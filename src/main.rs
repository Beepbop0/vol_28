mod app;
mod build_db;
mod view;

use anyhow::Context;
use std::env;
use std::path::PathBuf;

pub const DB_PATH: &str = "library.db";

fn basic_mode() -> anyhow::Result<()> {
    let mut args = env::args();

    match (args.next(), args.next().as_deref()) {
        (Some(_), Some("tui")) => {
            crate::view::run_tui().context("error encountered when running TUI")?;
        }
        (Some(_), Some("shell")) => {
            crate::app::run_shell().context("error encountered when running shell")?;
        }
        (Some(_), Some("scan")) => {
            let Some(music_dir) = args.next() else {
                anyhow::bail!("expected path to a music directory to scan");
            };

            let music_dir = PathBuf::from(music_dir);

            build_db::build_db(&music_dir)?;
        }
        (Some(prog), _) => {
            eprintln!(
                "Usage: {} <tui> | <shell> | <scan> <path_to_music_library>",
                prog
            )
        }
        _ => eprintln!("how did you even call this program?!"),
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    basic_mode()
}
