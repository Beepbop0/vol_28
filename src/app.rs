use anyhow::{Context, Result};
use rusqlite::Connection;
use std::io::{self, Write};
use std::iter::Peekable;
use std::sync::mpsc;
use std::thread;
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

pub struct LogLine {
    pub is_stderr: bool,
    pub line: String,
}

pub enum LogMessage {
    Line(LogLine),
    Complete(Result<String>),
}

impl From<LogLine> for LogMessage {
    fn from(line: LogLine) -> Self {
        LogMessage::Line(line)
    }
}

impl From<Result<String>> for LogMessage {
    fn from(result: Result<String>) -> Self {
        LogMessage::Complete(result)
    }
}

#[derive(Debug, Clone)]
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

// DB Queries
pub mod queries {
    use super::Song;
    use anyhow::{Context, Result};
    use rusqlite::{Connection, params};

    pub fn track_from_row<'a>(row: &rusqlite::Row<'a>) -> rusqlite::Result<Song> {
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

    pub fn track_from_id(conn: &Connection, id: i64) -> Result<Song> {
        let sql = "SELECT id, path, title, artist, album, track, year, duration_sec FROM tracks WHERE id = ?1";
        conn.query_row(sql, params![id], track_from_row)
            .with_context(|| format!("Track ID {} not found in database.", id))
    }

    pub fn list_artists(conn: &Connection) -> Result<Vec<String>> {
        let mut stmt = conn
            .prepare("SELECT DISTINCT artist FROM tracks ORDER BY artist")
            .context("failed to prepare query to list all artists")?;
        stmt.query_map([], |row| row.get::<_, _>(0))
            .context("failed to query database")?
            .collect::<Result<Vec<_>, _>>()
            .context("failed to map artists from database to strings")
    }

    pub fn list_album(conn: &Connection, album: &str) -> Result<Vec<Song>> {
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

    pub fn list_artist_tracks(conn: &Connection, artist: &str) -> Result<Vec<Song>> {
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
    pub fn search_group(conn: &Connection, terms: &str) -> anyhow::Result<Vec<Song>> {
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
}

pub struct AppState {
    conn: Connection,
    playlist: Vec<Song>,
}

impl AppState {
    pub fn new() -> Result<Self> {
        // Connect to the database
        let conn = Connection::open(DB_PATH)
            .context("Failed to open library.db. Ensure it is created and populated.")?;

        Ok(AppState {
            conn,
            playlist: Vec::new(),
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn playlist(&self) -> &[Song] {
        &self.playlist
    }

    pub fn playlist_add_by_id(&mut self, id: i64) -> Result<()> {
        let track = queries::track_from_id(&self.conn, id)?;

        self.playlist_add(track)
    }

    pub fn playlist_add(&mut self, song: Song) -> Result<()> {
        if playlist_duration_secs(&self.playlist[..]) + song.duration_sec > CD_MAX_DURATION_SECONDS
        {
            anyhow::bail!(
                "Track is too long! Adding would exceed the CD Limit of {}",
                humantime_secs(CD_MAX_DURATION_SECONDS)
            );
        }

        self.playlist.push(song);

        Ok(())
    }

    pub fn playlist_remove(&mut self, index: usize) -> bool {
        if index >= self.playlist.len() {
            return false;
        }
        self.playlist.remove(index);

        true
    }

    pub fn playlist_clear(&mut self) {
        self.playlist.clear();
    }

    pub fn burn(&self) -> Result<(thread::JoinHandle<Result<()>>, mpsc::Receiver<LogMessage>)> {
        let (tx, rx) = mpsc::channel();
        let playlist = self.playlist().to_vec();
        let handle = thread::spawn(move || -> Result<()> {
            playlist_burn(playlist, tx).context("failed to burn playlist")
        });

        Ok((handle, rx))
    }
}

/// Executes the final normalization and burning pipeline.
// - Downsample + decompress music
// - Normalize
// - Burn to CD
pub fn playlist_burn(playlist: Vec<Song>, msgs: mpsc::Sender<LogMessage>) -> Result<()> {
    use LogMessage::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    let temp_dir = match temp_dir() {
        Ok(dir) => dir,
        Err(err) => {
            msgs.send(Err(anyhow::anyhow!("failed to setup tempdir: {:?}", err)).into())
                .context("failed to send")?;
            return Ok(());
        }
    };

    if playlist.is_empty() {
        msgs.send(Complete(Err(anyhow::anyhow!(
            "playlist is empty. Add songs first"
        ))))
        .context("failed to send")?;
        return Ok(());
    }

    let mut downsampled_paths = std::collections::HashMap::new();

    for song in &playlist {
        if downsampled_paths.contains_key(&song.id) {
            continue;
        }

        msgs.send(
            LogLine {
                is_stderr: false,
                line: format!("transcoding track {}...", song.title),
            }
            .into(),
        )
        .context("failed to send")?;
        let song_path = &song.path;

        // 3. Transcode and Downsample (FFmpeg)
        let output_filename = format!("track_{}.wav", song.id);
        let output_path = temp_dir.path().join(&output_filename);

        let status = Command::new("ffmpeg")
            .arg("-i")
            .arg(song_path)
            .arg("-y")
            .arg("-ar")
            .arg("44100")
            .arg("-ac")
            .arg("2")
            .arg("-sample_fmt")
            .arg("s16")
            .arg(&output_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("FFmpeg failed for source path: {}", song_path))?;

        if !status.success() {
            msgs.send(Err(anyhow::anyhow!("ffmpeg failed to transcode track at path {}. Check source file access and validity.",
                song_path
            )).into()).context("failed to send")?;
            return Ok(());
        }
        downsampled_paths.insert(song.id, output_path);
    }

    let wav_files = downsampled_paths.values().cloned().collect::<Vec<_>>();

    let mut normalize_command = Command::new("normalize");
    normalize_command
        .current_dir(temp_dir.path())
        .arg("-b")
        .args(wav_files)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let status = normalize_command
        .status()
        .context("Failed to execute normalize. Is it installed?")?;

    if !status.success() {
        msgs.send(LogMessage::Complete(Err(anyhow::anyhow!(
            "Audio normalization failed."
        ))))
        .context("failed to send")?;
        return Ok(());
    }

    msgs.send(
        LogLine {
            is_stderr: false,
            line: String::from("Normalized playlist volume"),
        }
        .into(),
    )
    .context("failed to send")?;

    let playlist_files = playlist
        .iter()
        .map(|song| downsampled_paths[&song.id].clone())
        .collect::<Vec<_>>();

    msgs.send(
        LogLine {
            is_stderr: false,
            line: String::from("Burning playlist"),
        }
        .into(),
    )
    .context("failed to send")?;

    let mut wodim = Command::new("wodim")
        .current_dir(temp_dir)
        .arg("-v")
        .arg("-eject")
        .arg("-dao")
        .arg("-pad")
        .arg("dev=")
        .arg(CD_WRITER_DEVICE)
        .arg("-audio")
        .args(playlist_files)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn wodim. Check device path and permissions.")?;

    let (stdout, stderr) = (
        wodim
            .stdout
            .take()
            .context("failed to get handle to stdout")?,
        wodim
            .stderr
            .take()
            .context("failed to get handle to stderr")?,
    );

    let (stdout, stderr) = (BufReader::new(stdout), BufReader::new(stderr));

    let mut handles = vec![];
    let stdout_sender = msgs.clone();
    handles.push(thread::spawn(move || -> Result<()> {
        for line in stdout.lines() {
            let line = line.context("failed to obtain line from stdout")?;
            stdout_sender
                .send(
                    LogLine {
                        is_stderr: false,
                        line,
                    }
                    .into(),
                )
                .context("failed to send")?;
        }
        Ok(())
    }));

    let stderr_sender = msgs.clone();
    handles.push(thread::spawn(move || -> Result<()> {
        for line in stderr.lines() {
            let line = line.context("failed to obtain line from stderr")?;
            stderr_sender
                .send(
                    LogLine {
                        is_stderr: false,
                        line,
                    }
                    .into(),
                )
                .context("failed to send")?;
        }
        Ok(())
    }));

    let status = wodim.wait().context("failed to wait for wodim to exit")?;

    if !status.success() {
        msgs.send(Err(anyhow::anyhow!("failed to burn playlist")).into())
            .context("failed to send")?;
        return Ok(());
    }

    msgs.send(Ok(String::from("✅ CD Burning Complete. Disc ejected.")).into())
        .context("failed to send")?;

    for handle in handles {
        if let Err(_) = handle.join() {
            anyhow::bail!("pipe failed");
        }
    }

    Ok(())
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

                state.playlist_add_by_id(id)?;
            }
            Some("clear") => {
                state.playlist_clear();
                println!("playlist has been cleared");
            }
            Some("burn") => {
                let (handle, rx) = state.burn().context("failed to setup burning task")?;

                while let Ok(msg) = rx.recv() {
                    match msg {
                        LogMessage::Line(LogLine { is_stderr, line }) => {
                            if is_stderr {
                                eprintln!("{}", line)
                            } else {
                                println!("{}", line)
                            }
                        }
                        LogMessage::Complete(result) => {
                            let output = result?;
                            println!("{}", output);
                        }
                    }
                }

                if let Err(_) = handle.join() {
                    eprintln!("failed to join on burning playlist thread");
                }
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
            let tracks = queries::search_group(&state.conn, join_strings(parts).as_str())?;

            print_tracks(&tracks[..]);
        }
        "artist-list" => {
            if parts.peek().is_none() {
                let artists = queries::list_artists(&state.conn)?;
                println!("artists");
                for artist in artists {
                    println!("{}", artist);
                }
            } else {
                let artist = join_strings(parts);
                println!("tracks from artist \"{}\"", artist);
                let tracks = queries::list_artist_tracks(&state.conn, &artist[..])?;
                print_tracks(&tracks[..]);
            }
        }
        "album-list" => {
            if parts.peek().is_none() {
                anyhow::bail!("need an album to list");
            }
            let album = join_strings(parts);
            let tracks = queries::list_album(&state.conn, &album)?;
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
