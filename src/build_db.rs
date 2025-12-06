use anyhow::Context;
use lofty::{file::TaggedFile, probe::Probe};
use rusqlite::{Connection, Transaction};
use std::path::Path;
use walkdir::WalkDir;

use crate::DB_PATH;

pub type CowStr<'a> = std::borrow::Cow<'a, str>;

/// The structure representing the data we store in the database.
#[derive(Debug)]
pub struct InsertSong<'a> {
    pub path: CowStr<'a>,
    pub title: CowStr<'a>,
    pub artist: CowStr<'a>,
    pub track: u32,
    pub album: CowStr<'a>,
    pub year: u32,
    pub duration_sec: u64,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub bit_depth: u8,
}

const CREATE_TRACKS_SQL: &str = "
    CREATE TABLE IF NOT EXISTS tracks (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL UNIQUE,
        title TEXT,
        artist TEXT,
        track INTEGER,
        album TEXT,
        year INTEGER,
        duration_sec INTEGER,
        bit_depth INTEGER,
        bitrate_kbps INTEGER,
        sample_rate_hz INTEGER
    );
";
const INSERT_TRACK_SQL: &str = "
    INSERT INTO tracks (path, title, artist, track, album, year, duration_sec, bit_depth, bitrate_kbps, sample_rate_hz)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
";
const CREATE_TRACKS_FTS_SQL: &str = "
    CREATE VIRTUAL TABLE tracks_fts
    USING fts5 (
        id, title, artist, album
    );
";
const INSERT_TRACKS_FTS_SQL: &str = "
    INSERT INTO tracks_fts (id, title, artist, album)
    SELECT id, title, artist, album
    FROM tracks;
";

pub fn build_db(music_dir: &Path) -> anyhow::Result<()> {
    let mut conn = Connection::open(DB_PATH)
        .with_context(|| format!("failed to open db at path \"{DB_PATH}\""))?;

    build_tracks_table(&mut conn, music_dir).context("failed to create table \"tracks\"")?;

    Ok(())
}

fn build_tracks_table(conn: &mut Connection, music_dir: &Path) -> anyhow::Result<()> {
    conn.execute(CREATE_TRACKS_SQL, ())?;

    // tracks table
    {
        let tx = conn
            .transaction()
            .context("failed to obtain transaction for building tracks table")?;

        let results = scan_and_insert_in_transaction(&tx, music_dir)?;

        for error in results.read_errors {
            println!("encountered an error when scanning the library: {}", error);
        }
        println!("inserted {} tracks", results.inserted_count);

        tx.commit()?;
    }

    // full-text search table (fts)
    {
        let tx = conn
            .transaction()
            .context("failed to obtain transaction for building fts table")?;

        tx.execute(CREATE_TRACKS_FTS_SQL, ())
            .context("failed to execute creating fts table")?;

        tx.execute(INSERT_TRACKS_FTS_SQL, ())
            .context("failed to build fts table from tracks table")?;

        tx.commit().context("failed to commit fts table")?;
    }

    Ok(())
}

#[derive(Debug)]
struct TracksResults {
    inserted_count: usize,
    read_errors: Vec<anyhow::Error>,
}

/// Scans the directory, extracts metadata, and inserts into the database.
fn scan_and_insert_in_transaction(
    tx: &Transaction,
    root_dir: &Path,
) -> anyhow::Result<TracksResults> {
    let mut stmt = tx
        .prepare_cached(INSERT_TRACK_SQL)
        .context("failed to obtain cached statement for inserting track")?;
    let mut inserted_count = 0;
    let mut read_errors = vec![];

    println!("Scanning directory: {}...", root_dir.display());

    for entry in WalkDir::new(root_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            // Check for common music extensions before probing
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };

            if !matches!(ext, "mp3" | "flac" | "ogg" | "m4a") {
                continue;
            }

            // Probe the file and extract metadata using lofty
            match Probe::open(path).and_then(|p| p.read()) {
                Ok(tagged_file) => {
                    let track = match song_from_tags(&tagged_file, path) {
                        Ok(track) => track,
                        Err(e) => {
                            read_errors.push(anyhow::format_err!(
                                "failed to obtain tags/properties for {}: {}",
                                path.display(),
                                e
                            ));
                            continue;
                        }
                    };

                    // Insert the track data into the prepared statement
                    stmt.execute((
                        &track.path,
                        &track.title,
                        &track.artist,
                        &track.track,
                        &track.album,
                        &track.year,
                        &track.duration_sec,
                        &track.bit_depth,
                        &track.bitrate_kbps,
                        &track.sample_rate_hz,
                    ))
                    .with_context(|| {
                        format!("failed to insert the following track: {:?}", &track)
                    })?;
                    inserted_count += 1;
                }
                Err(e) => {
                    read_errors.push(anyhow::format_err!(
                        "failed to read tags for {}: {}",
                        path.display(),
                        e
                    ));
                }
            }
        }
    }

    Ok(TracksResults {
        inserted_count,
        read_errors,
    })
}

/// Helper function to safely extract data from lofty's structures.
fn song_from_tags<'a>(
    tagged_file: &'a TaggedFile,
    path: &'a Path,
) -> anyhow::Result<InsertSong<'a>> {
    use lofty::{
        file::{AudioFile, TaggedFileExt},
        tag::Accessor,
    };
    use std::borrow::Cow::{Borrowed, Owned};

    let tag = tagged_file.primary_tag().context("failed to obtain tags")?;
    let properties = tagged_file.properties();

    let title = tag.title().context("failed to obtain title tag")?;
    let mut artist = tag.artist().context("failed to obtain artist tag")?;
    if artist == "Deadmau5" {
        artist = Owned(String::from("deadmau5"));
    }
    let album = tag.album().unwrap_or(Borrowed(""));
    let year = tag.year().unwrap_or(0);
    let track = tag.track().unwrap_or(0);

    let bitrate_kbps = properties
        .audio_bitrate()
        .context("failed to obtain bitrate kbps")?;
    let sample_rate_hz = properties
        .sample_rate()
        .context("failed to obtain sample rate")?;
    let bit_depth = properties.bit_depth().unwrap_or(0);

    Ok(InsertSong {
        path: path.to_string_lossy(),
        title,
        artist,
        track,
        album,
        year,
        duration_sec: properties.duration().as_secs(),
        bit_depth,
        bitrate_kbps,
        sample_rate_hz,
    })
}
