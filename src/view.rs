use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{prelude::*, widgets::*};
use std::borrow::Cow;
use std::io;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use crate::app::{self, AppState, LogLine, LogMessage, Song, queries};

// --- TUI APP STATE ---

#[derive(PartialEq)]
enum ActivePane {
    Artists,
    ArtistTracks,
    Playlist,
}

struct View {
    state: AppState,

    // UI Focus
    active_pane: ActivePane,

    artists: WrappingList<String>,

    tracks: WrappingTable<Song>,

    playlist: WrappingTableState,

    // Help Tabs
    help: &'static [&'static str],

    // Feedback
    status_msg: Cow<'static, str>,
}

struct WrappingList<T> {
    items: Vec<T>,
    state: ListState,
}

impl<T> WrappingList<T> {
    fn next(&mut self) -> usize {
        let i = self.state.selected().map_or(0, |i| {
            let i = i + 1;
            if i == self.items.len() { 0 } else { i }
        });

        self.state.select(Some(i));

        i
    }

    fn prev(&mut self) -> usize {
        let i = self.state.selected().map_or(0, |i| {
            i.checked_sub(1).unwrap_or_else(|| self.items.len() - 1)
        });

        self.state.select(Some(i));

        i
    }
}

#[derive(Default)]
struct WrappingTableState(TableState);

impl WrappingTableState {
    fn next<T>(&mut self, items: &[T]) -> usize {
        let i = self.0.selected().map_or(0, |i| {
            let i = i + 1;
            if i == items.len() { 0 } else { i }
        });

        self.0.select(Some(i));

        i
    }

    fn prev<T>(&mut self, items: &[T]) -> usize {
        let i = self
            .0
            .selected()
            .map_or(0, |i| i.checked_sub(1).unwrap_or_else(|| items.len() - 1));

        self.0.select(Some(i));
        i
    }

    fn selected(&self) -> usize {
        self.0.selected().unwrap_or(0)
    }
}

struct WrappingTable<T> {
    items: Vec<T>,
    state: WrappingTableState,
}

impl<T> WrappingTable<T> {
    fn next(&mut self) -> usize {
        self.state.next(&self.items[..])
    }

    fn prev(&mut self) -> usize {
        self.state.prev(&self.items[..])
    }
}

impl View {
    fn new(state: AppState) -> Result<Self> {
        // Initial Data Load
        let artists = queries::list_artists(state.conn())
            .context("failed to grab initial list of artists")?;
        let mut artist_state = ListState::default();
        if !artists.is_empty() {
            artist_state.select(Some(0));
        }

        Ok(Self {
            state,
            active_pane: ActivePane::Artists,
            artists: WrappingList {
                items: artists,
                state: ListState::default(),
            },
            tracks: WrappingTable {
                items: vec![],
                state: WrappingTableState::default(),
            },
            playlist: WrappingTableState::default(),
            help: &ARTIST_HELP[..],
            status_msg: Cow::Borrowed(
                "Welcome. Use Left/Right to switch columns. Enter to select.",
            ),
        })
    }

    fn load_selected_artist(&mut self, index: usize) {
        let selected_artist = &self.artists.items[index];
        match queries::list_artist_tracks(self.state.conn(), selected_artist) {
            Ok(tracks) => {
                self.tracks.items = tracks;
                self.tracks.state = WrappingTableState::default();
            }
            Err(err) => {
                self.status_msg = Cow::Owned(format!(
                    "failed to load tracks for artist \"{}\": {:?}",
                    selected_artist, err
                ))
            }
        }
    }

    fn add_current_track(&mut self) {
        let selected_track = self.tracks.state.selected();
        let selected_track = &self.tracks.items[selected_track];
        if let Err(err) = self.state.playlist_add(selected_track.clone()) {
            self.status_msg = Cow::Owned(err.to_string());
        }
    }

    fn clear_playlist(&mut self) {
        self.state.playlist_clear();
        self.playlist = WrappingTableState::default();
    }
}

// --- MAIN ENTRY ---

pub fn run_tui() -> Result<()> {
    // Terminal Init
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // App Init
    let state = AppState::new()?;
    let mut view = View::new(state)?;

    // Initial load
    view.load_selected_artist(0);

    let res = run_app(&mut terminal, &mut view);

    // Terminal Restore
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err);
    }

    Ok(())
}

const ARTIST_HELP: [&str; 4] = [
    "(ESC) Quit",
    "(→ / Enter) Tracks Tab",
    "(↑ / ↓) Navigate Artists",
    "Jump To A Letter",
];
const TRACK_HELP: [&str; 4] = [
    "(←) Artists Tab",
    "(↑ / ↓) Navigate Tracks",
    "(→) Playlist Tab",
    "(Enter) Add Track",
];
const PLAYLIST_HELP: [&str; 4] = [
    "(←) Tracks Tab",
    "(Backspace) Remove Track",
    "(B) Burn Playlist",
    "(C) Clear Playlist",
];

#[derive(Debug)]
enum BurnPhase {
    BuildingPlaylist,
    Burning {
        logs: Vec<ratatui::text::Line<'static>>,
        completed: bool,
        rx: mpsc::Receiver<LogMessage>,
        handle: Option<JoinHandle<Result<()>>>,
    },
    Completed {
        logs: Vec<ratatui::text::Line<'static>>,
    },
}

fn to_ratatui_line(result: Result<String>) -> ratatui::text::Line<'static> {
    let (line, style) = match result {
        Ok(text) => (text, Style::default().fg(Color::Green)),
        Err(err) => (format!("{:?}", err), Style::default().fg(Color::Red)),
    };

    Line::from(vec![Span::styled(line, style)])
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, view: &mut View) -> Result<()> {
    use ratatui::text::{Line, Span};

    let mut burn_view = BurnPhase::BuildingPlaylist;

    loop {
        {
            use BurnPhase::*;
            use crossterm::event::KeyEvent;
            use std::time::Duration;

            // render the specific burn view if we are in a burning phase
            match &mut burn_view {
                Burning {
                    logs,
                    completed,
                    rx,
                    handle,
                } => {
                    // update our log lines
                    while let Ok(log_msg) = rx.try_recv() {
                        match log_msg {
                            LogMessage::Complete(result) => {
                                logs.push(to_ratatui_line(result));
                                logs.push(Line::from(vec![Span::styled(
                                    "Press 'Q' to build a new playlist",
                                    Style::default().fg(Color::White),
                                )]));

                                // SAFETY: assuming that we are receiving messages, it means we have an open thread handle to clean up.
                                let final_result = match handle.take().unwrap().join() {
                                    Ok(result) => result,
                                    Err(err) => anyhow::bail!(
                                        "failed to join background burn thread: {:?}",
                                        err
                                    ),
                                };

                                logs.push(to_ratatui_line(final_result.map(|_| String::from(""))));

                                *completed = true;
                            }
                            LogMessage::Line(LogLine { is_stderr, line }) => {
                                let style = if is_stderr {
                                    Style::default().fg(Color::Red)
                                } else {
                                    Style::default().fg(Color::Green)
                                };
                                let text = Line::from(vec![Span::styled(line, style)]);
                                logs.push(text);
                            }
                        }
                    }

                    terminal.draw(|f| burn_ui(f, logs))?;
                    if *completed {
                        let mut old_lines = vec![];
                        std::mem::swap(&mut old_lines, logs);
                        burn_view = BurnPhase::Completed { logs: old_lines };
                    }
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Completed { logs } => {
                    terminal.draw(|f| burn_ui(f, logs))?;
                    if let Event::Key(KeyEvent {
                        code: KeyCode::Char('Q'),
                        ..
                    }) = event::read()?
                    {
                        burn_view = BurnPhase::BuildingPlaylist;
                    }
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                BuildingPlaylist => (),
            }
        }

        terminal.draw(|f| ui(f, view))?;

        let Event::Key(key) = event::read()? else {
            continue;
        };

        match view.active_pane {
            ActivePane::Artists => match key.code {
                KeyCode::Esc => return Ok(()),
                KeyCode::Right | KeyCode::Enter => {
                    view.active_pane = ActivePane::ArtistTracks;
                    view.tracks
                        .state
                        .0
                        .select(Some(view.tracks.state.selected()));
                }
                KeyCode::Up => {
                    let i = view.artists.prev();
                    view.load_selected_artist(i);
                }
                KeyCode::Down => {
                    let i = view.artists.next();
                    view.load_selected_artist(i);
                }
                KeyCode::Char(c) => {
                    let s = String::from(c);
                    let i = match view.artists.items.as_slice().binary_search(&s) {
                        Ok(i) | Err(i) => i,
                    };
                    view.artists.state.select(Some(i));
                }
                _ => (),
            },
            ActivePane::ArtistTracks => match key.code {
                KeyCode::Left => {
                    view.active_pane = ActivePane::Artists;
                }
                KeyCode::Right => {
                    view.active_pane = ActivePane::Playlist;
                    view.playlist.0.select(Some(view.playlist.selected()));
                }
                KeyCode::Up => {
                    view.tracks.prev();
                }
                KeyCode::Down => {
                    view.tracks.next();
                }
                KeyCode::Enter => {
                    view.add_current_track();
                }
                _ => (),
            },
            ActivePane::Playlist => match key.code {
                KeyCode::Left => {
                    view.active_pane = ActivePane::ArtistTracks;
                }
                KeyCode::Up => {
                    view.playlist.prev(view.state.playlist());
                }
                KeyCode::Down => {
                    view.playlist.next(view.state.playlist());
                }
                KeyCode::Backspace => {
                    let index = view.playlist.selected();
                    view.state.playlist_remove(index);
                }
                KeyCode::Char('C') => {
                    view.clear_playlist();
                }
                KeyCode::Char('B') => {
                    let (handle, rx) = view.state.burn().context("failed to setup burn task")?;
                    burn_view = BurnPhase::Burning {
                        logs: vec![],
                        completed: false,
                        rx,
                        handle: Some(handle),
                    };
                }

                _ => (),
            },
        }
    }
}

// --- UI RENDERING ---

fn playlist_song_to_row(s: &Song) -> Row<'_> {
    Row::new(vec![
        Cell::from(s.title.clone()),
        Cell::from(app::humantime_secs(s.duration_sec).to_string()),
    ])
}

fn song_to_row(s: &Song) -> Row<'_> {
    Row::new(vec![
        Cell::from(s.title.clone()),
        Cell::from(s.album.clone()),
        Cell::from(s.year.to_string()),
        Cell::from(app::humantime_secs(s.duration_sec).to_string()),
    ])
}

fn highlight_item_style() -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .bg(Color::DarkGray)
}

fn ui(f: &mut Frame, view: &mut View) {
    let highlight_item_style = highlight_item_style();
    let (artist_border, tracks_border, playlist_border) = {
        let [mut artist, mut tracks, mut playlist] = [Style::default(); 3];
        let border_ref = match view.active_pane {
            ActivePane::Artists => &mut artist,
            ActivePane::ArtistTracks => &mut tracks,
            ActivePane::Playlist => &mut playlist,
        };
        *border_ref = Style::default().fg(Color::Yellow);
        (artist, tracks, playlist)
    };
    // 1. Vertical Layout: Main Body vs Bottom Bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(3), // Bottom bar height
        ])
        .split(f.area());

    // 2. Horizontal Layout: Artist | Library | Playlist
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20), // Artists
            Constraint::Percentage(60), // Tracks
            Constraint::Percentage(20), // Playlist
        ])
        .split(chunks[0]);

    // --- ARTIST COLUMN ---
    let artists: Vec<ListItem> = view
        .artists
        .items
        .iter()
        .map(|a| ListItem::new(Line::from(a.as_str())))
        .collect();

    let artist_block = Block::default()
        .borders(Borders::ALL)
        .title(" Artists ")
        .border_style(artist_border);

    let artist_list = List::new(artists)
        .block(artist_block)
        .highlight_style(highlight_item_style);

    f.render_stateful_widget(artist_list, body_chunks[0], &mut view.artists.state);

    let library_rows: Vec<Row> = view.tracks.items.iter().map(song_to_row).collect();

    let library_table = Table::new(
        library_rows,
        [
            Constraint::Percentage(40), // Title
            Constraint::Percentage(40), // Album
            Constraint::Length(5),      // Year
            Constraint::Length(6),      // Time
        ],
    )
    .header(
        Row::new(vec!["Title", "Album", "Year", "Time"])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .bottom_margin(1),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Tracks ")
            .border_style(tracks_border),
    )
    .row_highlight_style(highlight_item_style);

    f.render_stateful_widget(library_table, body_chunks[1], &mut view.tracks.state.0);

    // --- PLAYLIST COLUMN ---
    let playlist_rows: Vec<Row> = view
        .state
        .playlist()
        .iter()
        .map(playlist_song_to_row)
        .collect();

    // Calculate total time
    let total_secs: u64 = view.state.playlist().iter().map(|s| s.duration_sec).sum();
    let playlist_title = format!(" Playlist ({}/80m) ", app::humantime_secs(total_secs));

    let playlist_table = Table::new(
        playlist_rows,
        [Constraint::Percentage(80), Constraint::Percentage(20)],
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(playlist_title)
            .border_style(playlist_border),
    )
    .row_highlight_style(highlight_item_style);
    f.render_stateful_widget(playlist_table, body_chunks[2], &mut view.playlist.0);

    // --- BOTTOM BAR ---
    view.help = match view.active_pane {
        ActivePane::Artists => &ARTIST_HELP[..],
        ActivePane::ArtistTracks => &TRACK_HELP[..],
        ActivePane::Playlist => &PLAYLIST_HELP[..],
    };
    let tabs = Tabs::new(view.help.iter().cloned())
        .block(Block::default().borders(Borders::ALL).title(" Actions "))
        .style(Style::default().fg(Color::White))
        .highlight_style(Style::default().fg(Color::White))
        .divider(Span::raw("|"));

    f.render_widget(tabs, chunks[1]);

    // Status Message Overlay (Right side of bottom bar, or specific line)
    // We can render a paragraph over the tabs or just append it.
    // Let's float it in the bottom right of the actions block
    let status = Paragraph::new(Span::styled(
        view.status_msg.clone(),
        Style::default().fg(Color::LightCyan),
    ))
    .alignment(Alignment::Right);

    // Render status inside the bottom chunk, but padded
    let status_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1].inner(Margin {
            vertical: 1,
            horizontal: 1,
        }));

    f.render_widget(status, status_area[1]);
}

fn burn_ui<'a>(f: &mut Frame, logs: &mut Vec<ratatui::text::Line<'a>>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(1),    // Logs
        ])
        .split(f.area());

    // Header
    let header = Paragraph::new("Process Monitor (Press 'q' to quit)")
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // Calculate scroll to keep view at the bottom
    let scroll_offset = if logs.len() as u16 > chunks[1].height - 2 {
        (logs.len() as u16) - (chunks[1].height - 2)
    } else {
        0
    };

    let logs_widget = Paragraph::new(logs.clone())
        .block(Block::default().title("Output Logs").borders(Borders::ALL))
        .scroll((scroll_offset, 0)); // Auto-scroll

    f.render_widget(logs_widget, chunks[1]);
}
