use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::io::{self, Write};
use std::iter::Peekable;
use std::process::Command;
use tempfile::TempDir;

use crate::DB_PATH;

const CD_MAX_DURATION_SECONDS: u64 = 4799; // 79:59
const CD_WRITER_DEVICE: &str = "/dev/sr0"; // Default Linux CD device

fn temp_dir() -> io::Result<TempDir> {
    tempfile::tempdir_in("/dev/shm")
}

pub fn humantime_secs(secs: u64) -> humantime::FormattedDuration {
    humantime::format_duration(std::time::Duration::from_secs(secs))
}

#[derive(Debug)]
pub struct Song {
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track: i64,
    pub year: u32,
    pub duration_sec: u64,
}

fn track_from_row<'a>(row: &rusqlite::Row<'a>) -> rusqlite::Result<Song> {
    Ok(Song {
        id: row.get(0)?,
        path: row.get(1)?,
        title: row.get(2)?,
        artist: row.get(3)?,
        album: row.get(4)?,
        track: row.get(5)?,
        year: row.get(6)?,
        duration_sec: row.get(7)?,
    })
}

// DB Queries
fn track_from_id(conn: &Connection, id: i64) -> Result<Song> {
    let sql = "SELECT id, path, title, artist, album, track, year, duration_sec FROM tracks WHERE id = ?1";
    conn.query_row(sql, params![id], track_from_row)
        .with_context(|| format!("Track ID {} not found in database.", id))
}

fn list_artists(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT artist FROM tracks ORDER BY artist")
        .context("failed to prepare query to list all artists")?;
    stmt.query_map([], |row| row.get::<_, _>(0))
        .context("failed to query database")?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to map artists from database to strings")
}

fn list_album(conn: &Connection, album: &str) -> Result<Vec<Song>> {
    let mut stmt = conn
        .prepare(
            "SELECT
        id, path, title, artist, album, track, year, duration_sec
        FROM tracks
        WHERE album = ?1
        ORDER BY track",
        )
        .context("failed to prepare query to list all tracks in album")?;
    stmt.query_map([album], track_from_row)
        .with_context(|| format!("faield to query database for album \"{}\"", album))?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to map tracks from database to rust types")
}

fn list_artist_tracks(conn: &Connection, artist: &str) -> Result<Vec<Song>> {
    let mut stmt = conn
        .prepare(
            "SELECT 
    id, path, title, artist, album, track, year, duration_sec 
    FROM tracks
    WHERE artist = ?1
    ORDER BY year, album, track",
        )
        .context("failed to prepare query to list all artist's tracks")?;
    stmt.query_map([artist], track_from_row)
        .with_context(|| format!("failed to query database for artist \"{}\"", artist))?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to map tracks from database to rust types")
}

/// Clears the playlist and the staging directory.
fn search_group(conn: &Connection, terms: &str) -> anyhow::Result<Vec<Song>> {
    println!("searching for term \"{}\"", terms);
    let sql = r#"SELECT 
        t.id, t.path, t.title, t.artist, t.album, t.track, t.year, t.duration_sec
        FROM tracks AS t
        INNER JOIN tracks_fts AS f
        ON f.id = t.id
        WHERE tracks_fts MATCH '"' || ?1 || '"'
        LIMIT 50"#;

    let mut stmt = conn
        .prepare(sql)
        .context("failed to create search statement")?;

    stmt.query_map([terms], track_from_row)
        .with_context(|| format!("failed to query database with search term: \"{}\"", terms))?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to map tracks from database to rust types")
}

impl Song {
    pub fn format(&self) -> &str {
        self.path
            .rsplit_once('.')
            .map_or("unknown", |(_, extension)| extension)
    }
}

fn playlist_duration_secs(playlist: &[Song]) -> u64 {
    playlist.iter().fold(0u64, |acc, s| acc + s.duration_sec)
}

pub struct AppState {
    conn: Connection,
    playlist: Vec<Song>,
    staging_dir: TempDir,
}

impl AppState {
    pub fn new() -> Result<Self> {
        // Connect to the database
        let conn = Connection::open(DB_PATH)
            .context("Failed to open library.db. Ensure it is created and populated.")?;

        // Use tempfile which creates a secure, temporary directory that gets deleted
        // when it goes out of scope (usually placed in /tmp, which is often tmpfs).
        let staging_dir = temp_dir().context("Failed to create temporary staging directory")?;

        println!("Staging area: {}", staging_dir.path().display());

        Ok(AppState {
            conn,
            playlist: Vec::new(),
            staging_dir,
        })
    }

    /// Adds a track to the playlist, transcodes it, and checks CD capacity.
    pub fn playlist_add(&mut self, id: i64) -> Result<()> {
        // 1. Retrieve the full track data from DB
        let track = track_from_id(&self.conn, id)?;

        let track_path = &track.path;

        // 2. Check CD capacity
        if playlist_duration_secs(&self.playlist[..]) + track.duration_sec > CD_MAX_DURATION_SECONDS
        {
            anyhow::bail!(
                "Track is too long! Adding would exceed the CD Limit of {} CD limit.",
                humantime_secs(CD_MAX_DURATION_SECONDS)
            );
        }

        // 3. Transcode and Downsample (FFmpeg)
        // This is done BEFORE adding to the playlist state to catch immediate file access errors.
        println!("  -> Transcoding and validating file...");

        let output_filename = format!("track_{:02}_{}.wav", self.playlist.len() + 1, track.id);
        let output_path = self.staging_dir.path().join(&output_filename);

        let status = Command::new("ffmpeg")
            .arg("-i")
            .arg(track_path)
            .arg("-y")
            .arg("-ar")
            .arg("44100")
            .arg("-ac")
            .arg("2")
            .arg("-sample_fmt")
            .arg("s16")
            .arg(&output_path)
            .stdout(std::process::Stdio::null())
            .status()
            .with_context(|| format!("FFmpeg failed for source path: {}", track_path))?;

        if !status.success() {
            anyhow::bail!(
                "ffmpeg failed to transcode track at path {}. Check source file access and validity.",
                track_path
            );
        }

        // 4. Update state
        self.playlist.push(track);
        println!(
            "✅ Added track ID {} to playlist. Current duration: {}",
            id,
            humantime_secs(playlist_duration_secs(&self.playlist[..]))
        );

        Ok(())
    }

    pub fn playlist_clear(&mut self) -> Result<()> {
        // By creating a new TempDir, the old one is automatically deleted.
        let new_dir = temp_dir().context("failed to reset staging directory.")?;

        self.staging_dir = new_dir;
        self.playlist.clear();

        Ok(())
    }

    /// Executes the final normalization and burning pipeline.
    // TODO: make it so this does everything at once:
    // - Downsample + decompress music
    // - Normalize
    // - Burn to CD
    // TODO convert this to get the stdout pipe of the process that's running so we can render a view with the playlist burning
    pub fn playlist_burn(&mut self) -> Result<()> {
        if self.playlist.is_empty() {
            anyhow::bail!("Playlist is empty. Add songs first.");
        }

        let staging_path = self.staging_dir.path();
        let mut wav_files = vec![];
        for entry in std::fs::read_dir(staging_path)? {
            let entry = entry.with_context(|| {
                format!(
                    "failed to read entry in staging path: {}",
                    staging_path.display()
                )
            })?;
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "wav") {
                // pushing the filename is usually sufficient if current_dir is set
                let wav_file = path
                    .file_name()
                    .with_context(|| {
                        format!("failed to get file name of file path: {}", path.display())
                    })?
                    .to_os_string();
                wav_files.push(wav_file);
            }
        }

        // sort them by playlist order
        wav_files.sort();

        let mut normalize_command = Command::new("normalize");
        normalize_command
            .current_dir(staging_path)
            .arg("-b")
            .args(wav_files.clone());

        println!("command to run: {:?}", normalize_command);

        let status = normalize_command
            .status()
            .context("Failed to execute normalize. Is it installed?")?;

        if !status.success() {
            anyhow::bail!("Audio normalization failed.");
        }

        println!("\n--- Stage 3: Burning Audio CD ---");
        let status = Command::new("wodim")
            .current_dir(staging_path)
            .arg("-v")
            .arg("-eject")
            .arg("-dao")
            .arg("-pad")
            .arg("dev=")
            .arg(CD_WRITER_DEVICE)
            .arg("-audio")
            .args(wav_files)
            .status()
            .context("Failed to execute wodim. Check device path and permissions.")?;

        if !status.success() {
            anyhow::bail!("CD burning failed (wodim exit code error).");
        }

        println!("\n✅ CD Burning Complete. Disc ejected.");
        Ok(())
    }
}

/// Prints the current playlist selection.
fn playlist_print(playlist: &[Song]) {
    println!(
        "\n--- Current Playlist ({} Tracks, {} Total) ---",
        playlist.len(),
        humantime_secs(playlist_duration_secs(playlist))
    );
    print_tracks(playlist);
    println!("----------------------------------------------------\n");
}

fn print_tracks(tracks: &[Song]) {
    use std::borrow::Cow;
    println!("ID\tArtist\tTitle\tAlbum\tTrack Number\tFormat\tYear\tLength");
    for s @ Song {
        id,
        artist,
        title,
        album,
        track,
        year,
        duration_sec,
        ..
    } in tracks
    {
        let mut album = album.as_str();
        if album == "" {
            album = "\t";
        }
        let track_no = if *track == 0 {
            Cow::Borrowed("\t")
        } else {
            Cow::Owned(track.to_string())
        };
        let format = s.format();
        let length = humantime_secs(*duration_sec);
        println!("{id}\t{artist}\t{title}\t{album}\t{track_no}\t{format}\t{year}\t{length}",);
    }
}

// --- MAIN SHELL LOOP ---

pub fn run_shell() -> anyhow::Result<()> {
    let mut state = AppState::new()?;
    let stdin = io::stdin();

    println!("\n--- Audio Burner Shell ---");
    println!("Type 'help' for commands.");

    loop {
        print!("audio_burner> ");
        io::stdout().flush()?;

        let mut input = String::new();
        if stdin.read_line(&mut input)? == 0 {
            // EOF detected (Ctrl+D)
            break;
        }

        let input = input.trim();
        let mut parts = input.split_whitespace().peekable();

        let Some(command) = parts.next() else {
            continue;
        };

        let result = handle_command(command, parts, &mut state);

        match result {
            Ok(true) => break,
            Err(e) => eprintln!("❌ Error: {:?}", e),
            _ => (),
        }
    }

    println!("\nGoodbye!");
    Ok(())
}

// Immediate goals: Make it so I can build a playlist in memory.
fn handle_command<'a, I: Iterator<Item = &'a str>>(
    command: &'a str,
    mut parts: Peekable<I>,
    state: &mut AppState,
) -> anyhow::Result<bool> {
    const HELP_STR: &'static str = r#"
Command:
  playlist                               - show current playlist
  playlist limit                         - show limit of playlist length
  playlist add <ID>                      - add song by DB ID (transcodes and checks capacity)
  playlist burn                          - burn your playlist to the CD
  playlist clear                         - clears the existing playlist
  artist-list <artist>                   - shows all tracks made by a given artist, or show all artists if none is supplied
  album-list <album>                     - shows all tracks that belong to a given album
  search <query>                         - search against artist / album track tags using full text search
"#;
    match command {
        "quit" | "exit" => return Ok(true),
        "help" => {
            println!("{}", HELP_STR);
        }
        "playlist" => match parts.next() {
            Some("add") => {
                let id = parts
                    .next()
                    .context("expected an integer ID to be provided")?
                    .parse()
                    .context("failed to parse ID as integer")?;

                state.playlist_add(id)?;
            }
            Some("clear") => {
                state.playlist_clear()?;
                println!("playlist has been cleared");
            }
            Some("burn") => {
                state.playlist_burn()?;
            }
            Option::None | Some("list") => {
                playlist_print(&state.playlist[..]);
            }
            Some(unknown) => anyhow::bail!(
                "unknown playlist command\"{}\": expected one of add / list / clear / burn",
                unknown
            ),
        },
        "search" => {
            let tracks = search_group(&state.conn, join_strings(parts).as_str())?;

            print_tracks(&tracks[..]);
        }
        "artist-list" => {
            if parts.peek().is_none() {
                let artists = list_artists(&state.conn)?;
                println!("artists");
                for artist in artists {
                    println!("{}", artist);
                }
            } else {
                let artist = join_strings(parts);
                println!("tracks from artist \"{}\"", artist);
                let tracks = list_artist_tracks(&state.conn, &artist[..])?;
                print_tracks(&tracks[..]);
            }
        }
        "album-list" => {
            if parts.peek().is_none() {
                anyhow::bail!("need an album to list");
            }
            let album = join_strings(parts);
            let tracks = list_album(&state.conn, &album)?;
            print_tracks(&tracks[..]);
        }
        _ => anyhow::bail!("Unknown command\n{}\n", HELP_STR),
    };

    Ok(false)
}

fn join_strings<'a, I: Iterator<Item = &'a str>>(mut iter: Peekable<I>) -> String {
    let mut result = String::new();
    while let Some(part) = iter.next() {
        result += part;
        if iter.peek().is_some() {
            result += " ";
        }
    }

    result
}
