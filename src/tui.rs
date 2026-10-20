//! Interactive shell over the same tool functions the CLI calls.

use std::{
    ops::ControlFlow,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{
        Block, Borders, Gauge, List, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        watch,
    },
    time::{self, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::{
    ffprobe::Cancelled,
    quote::{self, quote},
    tools::{
        self, Tool,
        chapters::{self, Outcome},
        sync_check::{self, Report, diagnose::Severity, thousands},
        transcribe,
    },
};

/// How often the running screen redraws to show progress.
const PROGRESS_REDRAW: Duration = Duration::from_millis(100);

const VIDEO_EXTENSIONS: &[&str] = &[
    "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mts", "ts", "webm", "wmv",
];

pub async fn run() -> Result<()> {
    let dir = std::env::current_dir().context("failed to read current directory")?;
    let files = list_videos(&dir)?;

    let (results_tx, results_rx) = mpsc::unbounded_channel();
    let app = App::new(dir, files, results_tx);

    // Also installs a panic hook that restores the terminal before printing the panic.
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal, results_rx).await;
    ratatui::restore();
    result
}

type ToolOutput = Vec<Line<'static>>;

/// How a tool run ended.
#[derive(Debug)]
enum Finished {
    Output(ToolOutput),

    /// The tool stopped before replacing this file.
    Exists(PathBuf),
}

/// The latest progress report from whichever tool is running.
#[derive(Debug, Clone, PartialEq)]
enum Progress {
    Scan(sync_check::Progress),
    Transcription(transcribe::Progress),
}

/// Tags a tool run, so a result from a cancelled run can't land on a later run's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunId(u64);

struct App {
    dir: PathBuf,
    files: Vec<PathBuf>,

    /// Lives outside `Screen` so the selection survives a round trip into a tool.
    file_list: ListState,

    screen: Screen,
    next_run: u64,
    results_tx: UnboundedSender<(RunId, Finished)>,

    /// Set by Ctrl-C while a tool runs. The TUI exits once the tool has cleaned up.
    quitting: bool,
}

enum Screen {
    PickFile,
    PickTool {
        file: PathBuf,
        list: ListState,
    },
    Running {
        file: PathBuf,
        tool: Tool,
        run: RunId,
        cancel: CancellationToken,

        /// Set once the user cancels. The screen stays until the tool has cleaned up, so a
        /// subprocess can't outlive its screen.
        cancelling: bool,

        /// Holds the latest report only, so the tool never waits on the UI.
        progress: watch::Receiver<Option<Progress>>,
    },
    ConfirmOverwrite {
        file: PathBuf,
        tool: Tool,
        output: PathBuf,
    },
    Output {
        file: PathBuf,
        tool: Tool,
        lines: ToolOutput,

        /// May overshoot; `draw_output` clamps it to the content.
        scroll: usize,

        /// Set by `draw_output` so paging can move a screenful.
        viewport: usize,
    },
}

impl App {
    fn new(
        dir: PathBuf,
        files: Vec<PathBuf>,
        results_tx: UnboundedSender<(RunId, Finished)>,
    ) -> Self {
        App {
            file_list: ListState::default().with_selected(first_index(&files)),
            dir,
            files,
            screen: Screen::PickFile,
            next_run: 0,
            results_tx,
            quitting: false,
        }
    }

    async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        mut results_rx: UnboundedReceiver<(RunId, Finished)>,
    ) -> Result<()> {
        let mut events = EventStream::new();
        let mut redraw = time::interval(PROGRESS_REDRAW);
        redraw.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            terminal.draw(|frame| self.draw(frame))?;

            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else { return Ok(()) };
                    if let Event::Key(key) = event?
                        && key.kind == KeyEventKind::Press
                        && self.on_key(key).is_break()
                    {
                        return Ok(());
                    }
                }
                Some((run, finished)) = results_rx.recv() => {
                    if self.on_result(run, finished).is_break() {
                        return Ok(());
                    }
                }
                // Wakes the loop only to redraw progress, and only while a tool runs.
                _ = redraw.tick(), if matches!(self.screen, Screen::Running { .. }) => {}
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> ControlFlow<()> {
        let quit = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
        if quit && !matches!(self.screen, Screen::Running { .. }) {
            return ControlFlow::Break(());
        }
        self.quitting |= quit;
        let back = quit || matches!(key.code, KeyCode::Esc | KeyCode::Char('q'));

        // Each arm returns the next screen, or None to stay put. Building the next screen from
        // clones lets the `&mut self.screen` borrow end before the assignment below.
        let next = match &mut self.screen {
            Screen::PickFile => {
                if back {
                    return ControlFlow::Break(());
                }
                navigate(&mut self.file_list, key.code);
                (key.code == KeyCode::Enter)
                    .then(|| self.file_list.selected())
                    .flatten()
                    .map(|i| Screen::PickTool {
                        file: self.files[i].clone(),
                        list: ListState::default().with_selected(first_index(&Tool::ALL)),
                    })
            }
            Screen::PickTool { file, list } => {
                if back {
                    Some(Screen::PickFile)
                } else {
                    navigate(list, key.code);
                    let selected = list.selected().and_then(|i| Tool::ALL.get(i));
                    match (key.code, selected) {
                        (KeyCode::Enter, Some(&tool)) => Some(start(
                            &mut self.next_run,
                            &self.results_tx,
                            file.clone(),
                            tool,
                            false,
                        )),
                        _ => None,
                    }
                }
            }
            Screen::ConfirmOverwrite { file, tool, .. } => match key.code {
                KeyCode::Char('y') => Some(start(
                    &mut self.next_run,
                    &self.results_tx,
                    file.clone(),
                    *tool,
                    true,
                )),
                KeyCode::Char('n') => Some(tool_picker(file.clone(), *tool)),
                _ if back => Some(tool_picker(file.clone(), *tool)),
                _ => None,
            },
            Screen::Running {
                cancel, cancelling, ..
            } => {
                // The task stops its subprocesses and cleans up, then its result moves this
                // screen on.
                if back {
                    cancel.cancel();
                    *cancelling = true;
                }
                None
            }
            Screen::Output {
                file,
                tool,
                scroll,
                viewport,
                ..
            } => {
                if back {
                    Some(tool_picker(file.clone(), *tool))
                } else {
                    // Keep one line of overlap so paging doesn't lose your place.
                    let page = viewport.saturating_sub(1).max(1);
                    match key.code {
                        KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                        KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                        KeyCode::PageDown | KeyCode::Char(' ') => {
                            *scroll = scroll.saturating_add(page);
                        }
                        KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
                        KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                        KeyCode::End | KeyCode::Char('G') => *scroll = usize::MAX,
                        _ => {}
                    }
                    None
                }
            }
        };

        if let Some(screen) = next {
            self.screen = screen;
        }
        ControlFlow::Continue(())
    }

    fn on_result(&mut self, run: RunId, finished: Finished) -> ControlFlow<()> {
        if let Screen::Running {
            file,
            tool,
            run: current,
            cancelling,
            ..
        } = &self.screen
            && *current == run
        {
            if self.quitting {
                return ControlFlow::Break(());
            }
            self.screen = match finished {
                _ if *cancelling => tool_picker(file.clone(), *tool),
                Finished::Output(lines) => Screen::Output {
                    file: file.clone(),
                    tool: *tool,
                    lines,
                    scroll: 0,
                    viewport: 0,
                },
                Finished::Exists(output) => Screen::ConfirmOverwrite {
                    file: file.clone(),
                    tool: *tool,
                    output,
                },
            };
        }
        ControlFlow::Continue(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let crumbs = match &self.screen {
            Screen::PickFile => {
                let prefix = HEADER_LOGO.chars().count() + HEADER_SEPARATOR.chars().count();
                let width = usize::from(header.width).saturating_sub(prefix);
                vec![Span::from(truncate_left(&tilde(&self.dir), width))]
            }
            Screen::PickTool { file, .. } => vec![File(file).span()],
            Screen::Running { file, tool, .. }
            | Screen::ConfirmOverwrite { file, tool, .. }
            | Screen::Output { file, tool, .. } => {
                vec![File(file).span(), Span::from(tool.name())]
            }
        };
        frame.render_widget(header_line(crumbs), header);

        let hints = match &mut self.screen {
            Screen::PickFile => {
                draw_files(frame, body, &self.files, &mut self.file_list);
                "↑↓ move · Enter open · q quit"
            }
            Screen::PickTool { list, .. } => {
                draw_tools(frame, body, list);
                "↑↓ move · Enter run · q back"
            }
            Screen::Running {
                file,
                tool,
                progress,
                cancelling,
                ..
            } => {
                let progress = progress.borrow().clone();
                draw_running(frame, body, *tool, file, progress, *cancelling);
                if *cancelling {
                    "Cancelling…"
                } else {
                    "Esc cancel"
                }
            }
            Screen::ConfirmOverwrite { tool, output, .. } => {
                let question = Line::from(vec![
                    File(output).span(),
                    Span::from(" already exists. Overwrite it?"),
                ]);
                let paragraph = Paragraph::new(question).wrap(Wrap { trim: true });
                frame.render_widget(paragraph.yellow().block(panel(tool.name())), body);
                "y overwrite · n back"
            }
            Screen::Output {
                tool,
                lines,
                scroll,
                viewport,
                ..
            } => {
                draw_output(frame, body, tool.name(), lines, scroll, viewport);
                "↑↓ PgUp PgDn scroll · q back"
            }
        };
        frame.render_widget(Line::from(hints).dark_gray(), footer);
    }
}

/// Starts `tool` on `file` and returns the screen that tracks it. `force` lets the tool replace
/// its existing output.
fn start(
    next_run: &mut u64,
    results_tx: &UnboundedSender<(RunId, Finished)>,
    file: PathBuf,
    tool: Tool,
    force: bool,
) -> Screen {
    let run = RunId(*next_run);
    *next_run += 1;
    let cancel = CancellationToken::new();
    let (progress_tx, progress) = watch::channel(None);
    spawn_tool(
        tool,
        file.clone(),
        force,
        run,
        cancel.clone(),
        progress_tx,
        results_tx.clone(),
    );
    Screen::Running {
        file,
        tool,
        run,
        cancel,
        cancelling: false,
        progress,
    }
}

fn tool_picker(file: PathBuf, tool: Tool) -> Screen {
    let index = Tool::ALL.iter().position(|&t| t == tool);
    Screen::PickTool {
        file,
        list: ListState::default().with_selected(index),
    }
}

fn first_index<T>(items: &[T]) -> Option<usize> {
    (!items.is_empty()).then_some(0)
}

fn navigate(list: &mut ListState, code: KeyCode) {
    match code {
        KeyCode::Down | KeyCode::Char('j') => list.select_next(),
        KeyCode::Up | KeyCode::Char('k') => list.select_previous(),
        KeyCode::Home | KeyCode::Char('g') => list.select_first(),
        KeyCode::End | KeyCode::Char('G') => list.select_last(),
        _ => {}
    }
}

/// Runs a tool in the background and sends its rendered output back to the event loop. The task
/// is detached; `cancel` stops it.
fn spawn_tool(
    tool: Tool,
    file: PathBuf,
    force: bool,
    run: RunId,
    cancel: CancellationToken,
    progress: watch::Sender<Option<Progress>>,
    tx: UnboundedSender<(RunId, Finished)>,
) {
    tokio::spawn(async move {
        let finished = match run_tool(tool, &file, force, &cancel, &progress).await {
            Ok(lines) => Finished::Output(lines),
            Err(e) => match e.downcast::<tools::Exists>() {
                Ok(tools::Exists(output)) => Finished::Exists(output),
                Err(e) => Finished::Output(error_lines(&format!("Error: {e:#}"))),
            },
        };

        // Fails only if the TUI has exited, in which case nobody needs the result.
        let _ = tx.send((run, finished));
    });
}

/// An error's message in red, then any detail below it, such as a stage's stderr, in gray.
fn error_lines(error: &str) -> ToolOutput {
    let mut lines = error.lines().map(message);
    let mut output: ToolOutput = lines.next().map(Line::red).into_iter().collect();
    output.extend(lines.map(Line::dark_gray));
    output
}

/// A tool's message as one line, with the paths it marked styled as files.
fn message(text: &str) -> Line<'static> {
    let spans: Vec<Span> = quote::runs(text)
        .map(|(run, path)| {
            if path {
                File::style(run)
            } else {
                Span::from(run.to_string())
            }
        })
        .collect();
    Line::from(spans)
}

async fn run_tool(
    tool: Tool,
    file: &Path,
    force: bool,
    cancel: &CancellationToken,
    progress: &watch::Sender<Option<Progress>>,
) -> Result<ToolOutput> {
    match tool {
        // Dropping the unfinished future on cancel kills ffprobe through `kill_on_drop`.
        Tool::Chapters => cancel
            .run_until_cancelled(chapters::run(file))
            .await
            .ok_or(Cancelled)?
            .map(|o| chapters_lines(file, o)),
        // `send_replace` succeeds even after the UI drops the receiver.
        Tool::SyncCheck => sync_check::run(file, cancel, |p| {
            progress.send_replace(Some(Progress::Scan(p)));
        })
        .await
        .map(|report| sync_check_lines(&report)),
        Tool::Transcribe => {
            transcribe::run(file, transcribe::Backend::Whisper, force, cancel, |p| {
                progress.send_replace(Some(Progress::Transcription(p)));
            })
            .await
            .map(|outcome| outcome.lines().iter().map(|line| message(line)).collect())
        }
    }
}

fn chapters_lines(file: &Path, outcome: Outcome) -> ToolOutput {
    match outcome {
        Outcome::NoChapters => {
            vec![Line::from(vec![File(file).span(), Span::from(" has no chapters.")]).yellow()]
        }
        Outcome::Chapters(report) => {
            let mut lines: Vec<Line> = report.lines().map(Line::from).collect();
            if !report.violations.is_empty() {
                lines.push(Line::default());
                lines.extend(
                    report
                        .violations
                        .iter()
                        .map(|v| Line::from(format!("warning: {v}")).yellow()),
                );
            }
            lines
        }
    }
}

fn sync_check_lines(report: &Report) -> ToolOutput {
    let mut lines: ToolOutput = report
        .header()
        .into_iter()
        .map(|line| Line::from(line).dark_gray())
        .collect();
    lines.push(Line::default());
    if report.findings.is_empty() {
        lines.push(Line::from(sync_check::NO_ISSUES).green());
    }
    // Blank lines keep wrapped findings apart.
    for (i, finding) in report.findings.iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        let line = Line::from(finding.to_string());
        lines.push(match finding.severity {
            Severity::Major => line.red(),
            Severity::Minor => line.yellow(),
        });
    }
    lines
}

const HEADER_LOGO: &str = " vt ";
const HEADER_SEPARATOR: &str = " › ";

fn header_line(crumbs: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::from(HEADER_LOGO).reversed().bold()];
    for crumb in crumbs {
        spans.push(Span::from(HEADER_SEPARATOR).dark_gray());
        spans.push(crumb);
    }
    Line::from(spans)
}

fn tilde(path: &Path) -> String {
    match std::env::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Keeps the end of `text`, the more specific part of a path, when it exceeds `width` chars.
fn truncate_left(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len <= width {
        return text.to_string();
    }
    let tail: String = text.chars().skip(len + 1 - width.max(1)).collect();
    format!("…{tail}")
}

fn panel(title: &str) -> Block<'_> {
    Block::new()
        .borders(Borders::TOP)
        .title(format!(" {title} "))
        .border_style(Style::new().dark_gray())
        .title_style(Style::new().add_modifier(Modifier::BOLD))
}

fn highlight() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn draw_files(frame: &mut Frame, area: Rect, files: &[PathBuf], state: &mut ListState) {
    let block = panel("Pick a file");
    if files.is_empty() {
        let text = format!("No video files here ({}).", VIDEO_EXTENSIONS.join(", "));
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
        frame.render_widget(paragraph.dark_gray().block(block), area);
        return;
    }
    let list = List::new(files.iter().map(|f| File(f).span()))
        .block(block)
        .highlight_style(highlight())
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, state);
}

fn draw_tools(frame: &mut Frame, area: Rect, state: &mut ListState) {
    let block = panel("Pick a tool");
    if Tool::ALL.is_empty() {
        frame.render_widget(
            Paragraph::new("No tools yet.").dark_gray().block(block),
            area,
        );
        return;
    }
    let items = Tool::ALL.iter().map(|tool| {
        Line::from(vec![
            Span::from(format!("{:<12}", tool.name())),
            Span::from(tool.summary()).dark_gray(),
        ])
    });
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight())
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, state);
}

fn draw_running(
    frame: &mut Frame,
    area: Rect,
    tool: Tool,
    file: &Path,
    progress: Option<Progress>,
    cancelling: bool,
) {
    let block = panel(tool.name());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [text, _, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let verb = if cancelling { "Cancelling" } else { "Running" };
    let status = Line::from(vec![
        Span::from(format!("{verb} {} on ", tool.name())),
        File(file).span(),
        Span::from("…"),
    ]);
    frame.render_widget(status, text);

    match progress {
        None => {}
        Some(Progress::Scan(progress)) => draw_scan_progress(frame, status_area, progress),
        Some(Progress::Transcription(progress)) => match progress.ratio() {
            Some(ratio) => frame.render_widget(gauge(ratio, progress.to_string()), status_area),
            None => frame.render_widget(Line::from(progress.to_string()), status_area),
        },
    }
}

fn draw_scan_progress(frame: &mut Frame, area: Rect, progress: sync_check::Progress) {
    let sync_check::Progress { packets, expected } = progress;
    let Some(expected) = expected.filter(|&n| n > 0) else {
        frame.render_widget(Line::from(format!("{} packets", thousands(packets))), area);
        return;
    };
    // The total is an estimate, so hold below 100% until the result replaces this screen.
    #[allow(clippy::cast_precision_loss)]
    let ratio = (packets as f64 / expected as f64).min(0.99);
    let label = format!(
        "{:.0}% · {} of ~{} packets",
        ratio * 100.0,
        thousands(packets),
        thousands(expected)
    );
    frame.render_widget(gauge(ratio, label), area);
}

fn gauge(ratio: f64, label: String) -> Gauge<'static> {
    Gauge::default()
        .ratio(ratio)
        .label(label)
        .gauge_style(Style::new().fg(Color::Cyan).bg(Color::DarkGray))
}

fn draw_output(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[Line<'static>],
    scroll: &mut usize,
    viewport: &mut usize,
) {
    // No side borders, so selecting text in the terminal copies clean lines.
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // A fixed scrollbar column keeps the wrap width, and so the row count, independent of
    // whether the scrollbar shows.
    let [text_area, scrollbar_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    let paragraph = Paragraph::new(lines.to_vec()).wrap(Wrap { trim: false });

    // Clamped here, where the viewport size is known, so key handlers can overshoot freely.
    *viewport = usize::from(text_area.height);
    let max_scroll = paragraph
        .line_count(text_area.width)
        .saturating_sub(*viewport);
    *scroll = (*scroll).min(max_scroll);

    let offset = u16::try_from(*scroll).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((offset, 0)), text_area);

    if max_scroll > 0 {
        // One position per possible scroll offset, so the thumb reaches the end at max_scroll.
        let mut state = ScrollbarState::new(max_scroll + 1).position(*scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            scrollbar_area,
            &mut state,
        );
    }
}

/// A file the TUI names, which it shows in bold wherever it appears.
struct File<'a>(&'a Path);

impl File<'_> {
    /// The file's name, without its directory.
    fn span(&self) -> Span<'static> {
        File::style(
            &self
                .0
                .file_name()
                .unwrap_or(self.0.as_os_str())
                .to_string_lossy(),
        )
    }

    /// `text` styled as a file, for a path a tool's message names.
    fn style(text: &str) -> Span<'static> {
        Span::from(text.to_string()).bold().fg(Color::Reset)
    }
}

fn list_videos(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to list {}", quote(dir)))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file() && is_video(path))
        .collect();
    files.sort();
    Ok(files)
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(tool: Tool, run: RunId, cancel: CancellationToken) -> App {
        let (results_tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(PathBuf::from("/videos"), vec![], results_tx);
        let (_, progress) = watch::channel(None);
        app.screen = Screen::Running {
            file: "/videos/stream.mkv".into(),
            tool,
            run,
            cancel,
            cancelling: false,
            progress,
        };
        app
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn output(line: &str) -> Finished {
        Finished::Output(vec![Line::from(line.to_string())])
    }

    #[test]
    fn ignores_results_from_other_runs() {
        let mut app = running(Tool::SyncCheck, RunId(1), CancellationToken::new());

        let _ = app.on_result(RunId(0), output("stale"));

        assert!(matches!(app.screen, Screen::Running { run: RunId(1), .. }));
    }

    #[test]
    fn esc_cancels_and_waits_for_the_tool_to_clean_up() {
        let cancel = CancellationToken::new();
        let mut app = running(Tool::Transcribe, RunId(0), cancel.clone());

        let flow = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(flow.is_continue());
        assert!(cancel.is_cancelled());
        assert!(matches!(
            app.screen,
            Screen::Running {
                cancelling: true,
                ..
            }
        ));
    }

    #[test]
    fn returns_to_the_tool_once_a_cancelled_run_finishes() {
        let mut app = running(Tool::Transcribe, RunId(0), CancellationToken::new());
        let _ = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));

        let flow = app.on_result(RunId(0), output("Error: cancelled"));

        assert!(flow.is_continue());
        let Screen::PickTool { list, .. } = &app.screen else {
            panic!("expected the tool picker");
        };
        assert_eq!(
            list.selected().map(|i| Tool::ALL[i]),
            Some(Tool::Transcribe)
        );
    }

    #[test]
    fn ctrl_c_quits_once_the_running_tool_has_cleaned_up() {
        let cancel = CancellationToken::new();
        let mut app = running(Tool::Transcribe, RunId(0), cancel.clone());

        let on_key = app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let on_result = app.on_result(RunId(0), output("Error: cancelled"));

        assert_eq!(
            (on_key, cancel.is_cancelled(), on_result),
            (ControlFlow::Continue(()), true, ControlFlow::Break(()))
        );
    }

    fn confirming(app: &mut App) {
        let _ = app.on_result(RunId(0), Finished::Exists("/videos/stream.srt".into()));
    }

    #[test]
    fn asks_before_overwriting_an_existing_output() {
        let mut app = running(Tool::Transcribe, RunId(0), CancellationToken::new());

        confirming(&mut app);

        let Screen::ConfirmOverwrite { tool, output, .. } = &app.screen else {
            panic!("expected the overwrite confirmation");
        };
        assert_eq!(
            (*tool, output.as_path()),
            (Tool::Transcribe, Path::new("/videos/stream.srt"))
        );
    }

    #[tokio::test]
    async fn reruns_the_tool_once_overwriting_is_confirmed() {
        let mut app = running(Tool::Transcribe, RunId(0), CancellationToken::new());
        confirming(&mut app);

        let _ = app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(matches!(
            app.screen,
            Screen::Running {
                tool: Tool::Transcribe,
                ..
            }
        ));
    }

    #[test]
    fn returns_to_the_tool_when_overwriting_is_declined() {
        let mut app = running(Tool::Transcribe, RunId(0), CancellationToken::new());
        confirming(&mut app);

        let _ = app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));

        assert!(matches!(app.screen, Screen::PickTool { .. }));
    }

    #[test]
    fn truncate_left_keeps_text_that_fits() {
        assert_eq!(truncate_left("/home/me", 8), "/home/me");
    }

    #[test]
    fn truncate_left_keeps_the_tail_within_width() {
        assert_eq!(truncate_left("/home/me/videos", 8), "…/videos");
    }

    #[test]
    fn recognises_video_extensions_case_insensitively() {
        assert!(is_video(Path::new("talk.mkv")));
        assert!(is_video(Path::new("TALK.MP4")));
        assert!(!is_video(Path::new("notes.txt")));
        assert!(!is_video(Path::new("mkv")));
    }
}
