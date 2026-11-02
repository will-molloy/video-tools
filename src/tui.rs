//! Interactive shell over the same tool functions the CLI calls.

use std::{
    collections::BTreeSet,
    ops::ControlFlow,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{StreamExt, stream};
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
    display,
    ffprobe::Cancelled,
    tools::{
        self, Tool,
        chapters::{self, Outcome},
        dialogue_separation::{self, Backend, Keep},
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

/// How a tool's run on one file ended.
#[derive(Debug)]
enum Finished {
    Output(ToolOutput),
    Failed(ToolOutput),

    /// The tool stopped before replacing this file.
    Exists(PathBuf),

    /// Cancelling or declining to overwrite stopped the tool on this file.
    Skipped,
}

/// News of one file in a run.
#[derive(Debug)]
enum FileEvent {
    Started(watch::Receiver<Option<Progress>>),
    Finished(Finished),
}

/// One file in a run.
#[derive(Debug)]
struct FileRun {
    file: PathBuf,
    state: FileState,
}

#[derive(Debug)]
enum FileState {
    Waiting,

    /// Holds the latest report only, so the tool never waits on the UI.
    Running(watch::Receiver<Option<Progress>>),

    Finished(Finished),
}

impl FileRun {
    fn waiting(file: PathBuf) -> Self {
        FileRun {
            file,
            state: FileState::Waiting,
        }
    }

    fn is_finished(&self) -> bool {
        matches!(self.state, FileState::Finished(_))
    }

    fn has_output(&self) -> bool {
        matches!(
            self.state,
            FileState::Finished(Finished::Output(_) | Finished::Failed(_))
        )
    }

    /// The output this file would replace, if it stopped to ask.
    fn existing_output(&self) -> Option<&Path> {
        match &self.state {
            FileState::Finished(Finished::Exists(output)) => Some(output),
            _ => None,
        }
    }
}

/// The latest progress report from whichever tool is running.
#[derive(Debug, Clone, PartialEq)]
enum Progress {
    Scan(sync_check::Progress),
    Transcription(transcribe::Progress),
    Separation(dialogue_separation::Progress),
}

/// Tags a tool run, so a result from a cancelled run can't land on a later run's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunId(u64);

struct App {
    dir: PathBuf,
    files: Vec<PathBuf>,

    // These live outside `Screen` so the picks survive a round trip into a tool.
    tool_list: ListState,
    file_list: ListState,
    checked: BTreeSet<usize>,
    keep: Keep,
    backend: Backend,
    option: OptionRow,

    screen: Screen,
    next_run: u64,
    results_tx: UnboundedSender<RunEvent>,

    /// Set by Ctrl-C while a tool runs. The TUI exits once the tool has cleaned up.
    quitting: bool,
}

/// News from a run, naming the file by its index in the run.
type RunEvent = (RunId, usize, FileEvent);

/// The pane of the picker that takes the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tools,
    Options,
    Files,
}

/// A row of the options for the picked tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionRow {
    Keep,
    Backend,
}

enum Screen {
    Pick {
        focus: Focus,
    },
    Running {
        job: Job,
        files: Vec<FileRun>,
        run: RunId,
        cancel: CancellationToken,

        /// Set once the user cancels. The screen stays until the tool has cleaned up, so a
        /// subprocess can't outlive its screen.
        cancelling: bool,
    },
    ConfirmOverwrite {
        job: Job,
        files: Vec<FileRun>,
    },
    Output {
        job: Job,
        files: Vec<FileRun>,
        lines: ToolOutput,

        /// May overshoot; `draw_output` clamps it to the content.
        scroll: usize,

        /// Set by `draw_output` so paging can move a screenful.
        viewport: usize,
    },
}

impl App {
    fn new(dir: PathBuf, files: Vec<PathBuf>, results_tx: UnboundedSender<RunEvent>) -> Self {
        App {
            tool_list: ListState::default().with_selected(first_index(&Tool::ALL)),
            file_list: ListState::default().with_selected(first_index(&files)),
            checked: BTreeSet::new(),
            keep: Keep::CHOICES[0],
            backend: Backend::CHOICES[0],
            option: OptionRow::Keep,
            dir,
            files,
            screen: Screen::Pick {
                focus: Focus::Tools,
            },
            next_run: 0,
            results_tx,
            quitting: false,
        }
    }

    async fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        mut results_rx: UnboundedReceiver<RunEvent>,
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
                Some((run, index, event)) = results_rx.recv() => {
                    if self.on_event(run, index, event).is_break() {
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
            Screen::Pick {
                focus: focus @ Focus::Tools,
            } => {
                if back {
                    return ControlFlow::Break(());
                }
                navigate(&mut self.tool_list, key.code);
                if matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
                    *focus = if has_options(&self.tool_list) {
                        Focus::Options
                    } else {
                        Focus::Files
                    };
                }
                None
            }
            Screen::Pick {
                focus: focus @ Focus::Options,
            } => {
                match key.code {
                    KeyCode::Enter | KeyCode::Tab => *focus = Focus::Files,
                    _ if back => *focus = Focus::Tools,
                    KeyCode::Up | KeyCode::Char('k') => self.option = OptionRow::Keep,
                    KeyCode::Down | KeyCode::Char('j') => self.option = OptionRow::Backend,
                    KeyCode::Left | KeyCode::Char('h') => self.change_option(false),
                    KeyCode::Right | KeyCode::Char('l' | ' ') => self.change_option(true),
                    _ => {}
                }
                None
            }
            Screen::Pick {
                focus: focus @ Focus::Files,
            } => {
                navigate(&mut self.file_list, key.code);
                match key.code {
                    KeyCode::Tab => *focus = Focus::Tools,
                    _ if back => {
                        *focus = if has_options(&self.tool_list) {
                            Focus::Options
                        } else {
                            Focus::Tools
                        };
                    }
                    KeyCode::Char(' ') => {
                        if let Some(i) = self.file_list.selected().filter(|&i| i < self.files.len())
                            && !self.checked.remove(&i)
                        {
                            self.checked.insert(i);
                        }
                    }
                    KeyCode::Char('a') => {
                        if self.checked.len() == self.files.len() {
                            self.checked.clear();
                        } else {
                            self.checked = (0..self.files.len()).collect();
                        }
                    }
                    _ => {}
                }
                let picked = picked(&self.files, &self.checked, &self.file_list);
                let tool = self.tool_list.selected().and_then(|i| Tool::ALL.get(i));
                match (key.code, tool) {
                    (KeyCode::Enter, Some(&tool)) if !picked.is_empty() => {
                        let mut start = |job| {
                            start(
                                &mut self.next_run,
                                &self.results_tx,
                                job,
                                picked.iter().cloned().map(FileRun::waiting).collect(),
                                false,
                            )
                        };
                        Some(match tool {
                            Tool::Chapters => start(Job::Chapters),
                            Tool::SyncCheck => start(Job::SyncCheck),
                            Tool::Transcribe => start(Job::Transcribe),
                            Tool::DialogueSeparation => {
                                start(Job::Separate(self.keep, self.backend))
                            }
                        })
                    }
                    _ => None,
                }
            }
            Screen::ConfirmOverwrite { job, files } => match key.code {
                KeyCode::Char('y') => {
                    let mut files = std::mem::take(files);
                    for run in &mut files {
                        if run.existing_output().is_some() {
                            run.state = FileState::Waiting;
                        }
                    }
                    Some(start(
                        &mut self.next_run,
                        &self.results_tx,
                        *job,
                        files,
                        true,
                    ))
                }
                KeyCode::Char('n') => Some(conclude(*job, std::mem::take(files))),
                _ if back => Some(conclude(*job, std::mem::take(files))),
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
                scroll, viewport, ..
            } => {
                if back {
                    Some(Screen::Pick {
                        focus: Focus::Files,
                    })
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

    /// Moves the focused option to its next or previous choice.
    fn change_option(&mut self, forward: bool) {
        match self.option {
            OptionRow::Keep => self.keep = cycle(&Keep::CHOICES, self.keep, forward),
            OptionRow::Backend => self.backend = cycle(&Backend::CHOICES, self.backend, forward),
        }
    }

    fn on_event(&mut self, run: RunId, index: usize, event: FileEvent) -> ControlFlow<()> {
        let Screen::Running {
            job,
            files,
            run: current,
            cancelling,
            ..
        } = &mut self.screen
        else {
            return ControlFlow::Continue(());
        };
        if *current != run {
            return ControlFlow::Continue(());
        }
        if let Some(file) = files.get_mut(index) {
            file.state = match event {
                FileEvent::Started(progress) => FileState::Running(progress),
                FileEvent::Finished(finished) => FileState::Finished(finished),
            };
        }
        if !files.iter().all(FileRun::is_finished) {
            return ControlFlow::Continue(());
        }
        if self.quitting {
            return ControlFlow::Break(());
        }
        let job = *job;
        let files = std::mem::take(files);
        self.screen = if !*cancelling && files.iter().any(|f| f.existing_output().is_some()) {
            Screen::ConfirmOverwrite { job, files }
        } else {
            conclude(job, files)
        };
        ControlFlow::Continue(())
    }

    /// Draws the tools, any options for the picked tool, then the files, marking the row that
    /// takes the keys in the focused pane.
    fn draw_picker(&mut self, frame: &mut Frame, area: Rect, focus: Focus) {
        let tools_height = u16::try_from(Tool::ALL.len()).unwrap_or(u16::MAX) + 1;
        let picked = focus != Focus::Tools;
        let options_height = if picked && has_options(&self.tool_list) {
            3
        } else {
            0
        };
        let [tools_area, options_area, files_area] = Layout::vertical([
            Constraint::Length(tools_height),
            Constraint::Length(options_height),
            Constraint::Fill(1),
        ])
        .areas(area);

        let rows = Tool::ALL.map(|t| (t.name(), t.summary()));
        let style = match focus {
            Focus::Tools => highlight(),

            // Keeps the picked tool visible while the keys move on.
            Focus::Options | Focus::Files => picked_style(),
        };
        draw_choices(
            frame,
            tools_area,
            panel("Pick a tool"),
            &rows,
            &mut self.tool_list,
            style,
        );
        if options_height > 0 {
            draw_options(
                frame,
                options_area,
                focus,
                self.keep,
                self.backend,
                self.option,
            );
        }
        draw_files(
            frame,
            files_area,
            focus,
            &self.files,
            &self.checked,
            &mut self.file_list,
        );
    }

    fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let crumbs = match &self.screen {
            Screen::Pick { .. } => {
                let prefix = HEADER_LOGO.chars().count() + HEADER_SEPARATOR.chars().count();
                let width = usize::from(header.width).saturating_sub(prefix);
                vec![Span::from(truncate_left(&tilde(&self.dir), width))]
            }
            Screen::Running { files, job, .. }
            | Screen::ConfirmOverwrite { files, job, .. }
            | Screen::Output { files, job, .. } => {
                let paths: Vec<PathBuf> = files.iter().map(|f| f.file.clone()).collect();
                let mut crumbs = vec![subject(&paths), job.tool().name().into()];
                crumbs.extend(subtitle(*job).map(Span::from));
                crumbs
            }
        };
        frame.render_widget(header_line(crumbs), header);

        let hints = match &mut self.screen {
            Screen::Pick { focus } => {
                let focus = *focus;
                self.draw_picker(frame, body, focus);
                match focus {
                    Focus::Tools => "↑↓ move · Enter next · q quit",
                    Focus::Options => "↑↓ move · ←→ change · Enter files · q back",
                    Focus::Files => "↑↓ move · Space select · a all · Enter run · q back",
                }
            }
            Screen::Running {
                job,
                files,
                cancelling,
                ..
            } => {
                draw_running(frame, body, &title(*job), files, *cancelling);
                if *cancelling {
                    "Cancelling…"
                } else {
                    "Esc cancel"
                }
            }
            Screen::ConfirmOverwrite { job, files } => {
                let mut question: Vec<Line> = files
                    .iter()
                    .filter_map(FileRun::existing_output)
                    .map(|output| {
                        Line::from(vec![File(output).display(), Span::from(" already exists.")])
                    })
                    .collect();
                question.push(Line::from(if question.len() == 1 {
                    "Overwrite it?"
                } else {
                    "Overwrite them?"
                }));
                let paragraph = Paragraph::new(question).wrap(Wrap { trim: true });
                let title = title(*job);
                frame.render_widget(paragraph.yellow().block(panel(&title)), body);
                "y overwrite · n back"
            }
            Screen::Output {
                job,
                lines,
                scroll,
                viewport,
                ..
            } => {
                draw_output(frame, body, &title(*job), lines, scroll, viewport);
                "↑↓ PgUp PgDn scroll · q back"
            }
        };
        frame.render_widget(Line::from(hints).dark_gray(), footer);
    }
}

/// The files a run covers, which are the checked ones, or the highlighted one if none are checked.
fn picked(files: &[PathBuf], checked: &BTreeSet<usize>, highlighted: &ListState) -> Vec<PathBuf> {
    if checked.is_empty() {
        highlighted
            .selected()
            .and_then(|i| files.get(i))
            .cloned()
            .into_iter()
            .collect()
    } else {
        checked.iter().map(|&i| files[i].clone()).collect()
    }
}

/// A crumb naming the files a run covers.
fn subject(files: &[PathBuf]) -> Span<'static> {
    match files {
        [file] => File(file).display(),
        files => Span::from(format!("{} files", files.len())),
    }
}

/// Ends a run with the output of its files, or back at the picker if it has no output to show.
fn conclude(job: Job, mut files: Vec<FileRun>) -> Screen {
    for run in &mut files {
        if run.existing_output().is_some() {
            run.state = FileState::Finished(Finished::Skipped);
        }
    }
    if !files.iter().any(FileRun::has_output) {
        return Screen::Pick {
            focus: Focus::Files,
        };
    }
    Screen::Output {
        job,
        lines: output_lines(&files),
        files,
        scroll: 0,
        viewport: 0,
    }
}

/// The output of each file, under its name when there are several.
fn output_lines(files: &[FileRun]) -> ToolOutput {
    if let [
        FileRun {
            state: FileState::Finished(Finished::Output(lines) | Finished::Failed(lines)),
            ..
        },
    ] = files
    {
        return lines.clone();
    }
    let mut output = vec![];
    for (i, run) in files.iter().enumerate() {
        if i > 0 {
            output.push(Line::default());
        }
        output.push(Line::from(File(&run.file).display()));
        match &run.state {
            FileState::Finished(Finished::Output(lines) | Finished::Failed(lines)) => {
                output.extend(lines.iter().cloned());
            }
            FileState::Finished(Finished::Exists(_) | Finished::Skipped)
            | FileState::Waiting
            | FileState::Running(_) => output.push(Line::from("Skipped.").dark_gray()),
        }
    }
    output
}

/// Whether the tool highlighted in `tool_list` has options to set.
fn has_options(tool_list: &ListState) -> bool {
    match tool_list.selected().and_then(|i| Tool::ALL.get(i)) {
        Some(Tool::DialogueSeparation) => true,
        Some(Tool::Chapters | Tool::SyncCheck | Tool::Transcribe) | None => false,
    }
}

/// The choice after `current`, or before it, wrapping around the ends.
fn cycle<T: Copy + PartialEq>(choices: &[T], current: T, forward: bool) -> T {
    let index = choices.iter().position(|&c| c == current).unwrap_or(0);
    let step = if forward { 1 } else { choices.len() - 1 };
    choices[(index + step) % choices.len()]
}

/// What a run does. Separation also needs the side and model the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Chapters,
    SyncCheck,
    Transcribe,
    Separate(Keep, Backend),
}

impl Job {
    fn tool(self) -> Tool {
        match self {
            Job::Chapters => Tool::Chapters,
            Job::SyncCheck => Tool::SyncCheck,
            Job::Transcribe => Tool::Transcribe,
            Job::Separate(..) => Tool::DialogueSeparation,
        }
    }
}

/// The choices below the tool's name, such as "Remove voice (TIGER-DnR)".
fn subtitle(job: Job) -> Option<String> {
    match job {
        Job::Separate(keep, backend) => Some(format!("{} ({})", keep.label(), backend.label())),
        Job::Chapters | Job::SyncCheck | Job::Transcribe => None,
    }
}

/// The run's panel title, the most specific of its crumbs.
fn title(job: Job) -> String {
    subtitle(job).unwrap_or_else(|| job.tool().name().into())
}

/// Spawns `job` as the next run on the waiting `files`, and returns the screen that shows it.
/// `force` lets the job replace existing outputs.
fn start(
    next_run: &mut u64,
    results_tx: &UnboundedSender<RunEvent>,
    job: Job,
    files: Vec<FileRun>,
    force: bool,
) -> Screen {
    let run = RunId(*next_run);
    *next_run += 1;
    let cancel = CancellationToken::new();
    let waiting = files
        .iter()
        .enumerate()
        .filter(|(_, file)| matches!(file.state, FileState::Waiting))
        .map(|(index, file)| (index, file.file.clone()))
        .collect();
    spawn_job(job, waiting, force, run, cancel.clone(), results_tx.clone());
    Screen::Running {
        job,
        files,
        run,
        cancel,
        cancelling: false,
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

/// Runs a job on `files`, each with its index in the run, up to the tool's parallel limit at once,
/// and sends each file's news back to the event loop. The next file starts as soon as any running
/// file finishes. The task is detached; `cancel` stops it and skips the files not yet started.
fn spawn_job(
    job: Job,
    files: Vec<(usize, PathBuf)>,
    force: bool,
    run: RunId,
    cancel: CancellationToken,
    tx: UnboundedSender<RunEvent>,
) {
    tokio::spawn(async move {
        let (cancel, tx) = (&cancel, &tx);
        stream::iter(files)
            .for_each_concurrent(job.tool().max_parallel(), |(index, file)| async move {
                // Sends fail only if the TUI has exited, in which case nobody needs the news.
                let send = |event| {
                    let _ = tx.send((run, index, event));
                };
                let finished = if cancel.is_cancelled() {
                    Finished::Skipped
                } else {
                    let (progress_tx, progress) = watch::channel(None);
                    send(FileEvent::Started(progress));
                    finished(run_job(job, &file, force, cancel, &progress_tx).await)
                };
                send(FileEvent::Finished(finished));
            })
            .await;
    });
}

fn finished(result: Result<ToolOutput>) -> Finished {
    match result {
        Ok(lines) => Finished::Output(lines),
        Err(e) if e.is::<Cancelled>() => Finished::Skipped,
        Err(e) => match e.downcast::<tools::Exists>() {
            Ok(tools::Exists(output)) => Finished::Exists(output),
            Err(e) => Finished::Failed(error_lines(&format!("Error: {e:#}"))),
        },
    }
}

/// An error's message in red, then any detail below it, such as `vt-ml`'s stderr, in gray.
fn error_lines(error: &str) -> ToolOutput {
    let mut lines = error.lines().map(message);
    let mut output: ToolOutput = lines.next().map(Line::red).into_iter().collect();
    output.extend(lines.map(Line::dark_gray));
    output
}

/// A tool's message as one line, with the paths it marked styled as files.
fn message(text: &str) -> Line<'static> {
    let spans: Vec<Span> = display::runs(text)
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

async fn run_job(
    job: Job,
    file: &Path,
    force: bool,
    cancel: &CancellationToken,
    progress: &watch::Sender<Option<Progress>>,
) -> Result<ToolOutput> {
    match job {
        // Dropping the unfinished future on cancel kills ffprobe through `kill_on_drop`.
        Job::Chapters => cancel
            .run_until_cancelled(chapters::run(file))
            .await
            .ok_or(Cancelled)?
            .map(|o| chapters_lines(file, o)),
        // `send_replace` succeeds even after the UI drops the receiver.
        Job::SyncCheck => sync_check::run(file, cancel, |p| {
            progress.send_replace(Some(Progress::Scan(p)));
        })
        .await
        .map(|report| sync_check_lines(&report)),
        Job::Transcribe => {
            transcribe::run(file, transcribe::Backend::Whisper, force, cancel, |p| {
                progress.send_replace(Some(Progress::Transcription(p)));
            })
            .await
            .map(|outcome| ml_lines(outcome.lines()))
        }
        Job::Separate(keep, backend) => {
            let transcript = transcribe::transcript_path(file);
            dialogue_separation::run(file, &transcript, keep, backend, force, cancel, |p| {
                progress.send_replace(Some(Progress::Separation(p)));
            })
            .await
            .map(|outcome| ml_lines(outcome.lines()))
        }
    }
}

/// An ML tool's outcome: what it wrote, then how it ran.
fn ml_lines(lines: Vec<String>) -> ToolOutput {
    let mut lines = lines.iter().map(|line| message(line));
    let mut output: ToolOutput = lines.next().map(Line::green).into_iter().collect();
    output.extend(lines.map(Line::dark_gray));
    output
}

fn chapters_lines(file: &Path, outcome: Outcome) -> ToolOutput {
    match outcome {
        Outcome::NoChapters => {
            vec![Line::from(vec![File(file).display(), Span::from(" has no chapters.")]).yellow()]
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

fn picked_style() -> Style {
    Style::new().fg(Color::Cyan)
}

/// Draws a row per option, with its choices and what the current one does.
fn draw_options(
    frame: &mut Frame,
    area: Rect,
    focus: Focus,
    keep: Keep,
    backend: Backend,
    row: OptionRow,
) {
    let line = |this: OptionRow, label: &str, choices: Vec<(&str, bool)>, summary: &str| {
        let focused = focus == Focus::Options && row == this;
        let mut spans = vec![Span::from(if focused { "› " } else { "  " })];
        spans.push(Span::from(format!("{label:<8}")));
        for (choice, chosen) in choices {
            spans.push(match (chosen, focused) {
                (true, true) => Span::styled(choice.to_string(), highlight()),
                (true, false) => Span::styled(choice.to_string(), picked_style()),
                (false, _) => Span::from(choice.to_string()),
            });
            spans.push(Span::from("  "));
        }
        spans.push(Span::from(summary.to_string()).dark_gray());
        Line::from(spans)
    };
    let lines = vec![
        line(
            OptionRow::Keep,
            "Action",
            Keep::CHOICES
                .iter()
                .map(|&k| (k.label(), k == keep))
                .collect(),
            keep.summary(),
        ),
        line(
            OptionRow::Backend,
            "Model",
            Backend::CHOICES
                .iter()
                .map(|&b| (b.label(), b == backend))
                .collect(),
            backend.summary(),
        ),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel("Options")), area);
}

fn draw_files(
    frame: &mut Frame,
    area: Rect,
    focus: Focus,
    files: &[PathBuf],
    checked: &BTreeSet<usize>,
    state: &mut ListState,
) {
    let title = if checked.is_empty() {
        "Pick files".to_string()
    } else {
        format!("Pick files ({} selected)", checked.len())
    };
    let block = panel(&title);
    if files.is_empty() {
        let text = format!("No video files here ({}).", VIDEO_EXTENSIONS.join(", "));
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: true });
        frame.render_widget(paragraph.dark_gray().block(block), area);
        return;
    }
    let rows = files.iter().enumerate().map(|(i, file)| {
        let mark = if checked.contains(&i) { "[x] " } else { "[ ] " };
        Line::from(vec![Span::from(mark), File(file).display()])
    });
    let list = List::new(rows).block(block);
    let list = match focus {
        Focus::Files => list.highlight_style(highlight()).highlight_symbol("› "),
        Focus::Tools | Focus::Options => list.highlight_symbol("  "),
    };
    frame.render_stateful_widget(list, area, state);
}

/// Lists names with their summaries in an aligned column.
fn draw_choices(
    frame: &mut Frame,
    area: Rect,
    block: Block<'static>,
    rows: &[(&str, &str)],
    state: &mut ListState,
    selected: Style,
) {
    // Two spaces between the longest name and its summary.
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 2;
    let items = rows.iter().map(|&(name, summary)| {
        Line::from(vec![
            Span::from(format!("{name:<width$}")),
            Span::from(summary.to_string()).dark_gray(),
        ])
    });
    let list = List::new(items)
        .block(block)
        .highlight_style(selected)
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, area, state);
}

/// Draws a status line, then a row per file with its progress.
fn draw_running(frame: &mut Frame, area: Rect, title: &str, files: &[FileRun], cancelling: bool) {
    let block = panel(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [status_area, _, rows_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let verb = if cancelling { "Cancelling" } else { "Running" };
    let done = files.iter().filter(|f| f.is_finished()).count();
    let status = format!("{verb} {title}… {done} of {} done", files.len());
    frame.render_widget(Line::from(status), status_area);

    // Half the width at most, so a long name leaves room for its progress.
    let name_width = files
        .iter()
        .map(|run| File(&run.file).display().width() + 2)
        .max()
        .unwrap_or(0)
        .min(usize::from(rows_area.width) / 2);
    let name_width = u16::try_from(name_width).unwrap_or(u16::MAX);

    let height = usize::from(rows_area.height);
    let first = first_visible(files, height);
    let rows = Layout::vertical(vec![Constraint::Length(1); height]).split(rows_area);
    for (run, &row) in files[first..].iter().zip(rows.iter()) {
        let [name_area, _, progress_area] = Layout::horizontal([
            Constraint::Length(name_width),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(row);
        let name = run.file.file_name().unwrap_or(run.file.as_os_str());
        let width = usize::from(name_width).saturating_sub(2);
        let name = Line::from(vec![
            state_mark(&run.state),
            File::style(&truncate_left(&name.to_string_lossy(), width)),
        ]);
        frame.render_widget(name, name_area);
        if let FileState::Running(progress) = &run.state {
            draw_progress(frame, progress_area, progress.borrow().clone());
        }
    }
}

/// The first row to show, so a batch taller than `height` keeps its running files in view.
fn first_visible(files: &[FileRun], height: usize) -> usize {
    let first_unfinished = files.iter().position(|f| !f.is_finished()).unwrap_or(0);
    first_unfinished.min(files.len().saturating_sub(height))
}

fn state_mark(state: &FileState) -> Span<'static> {
    match state {
        FileState::Waiting => Span::from("· ").dark_gray(),
        FileState::Running(_) => Span::from("› ").cyan(),
        FileState::Finished(Finished::Output(_)) => Span::from("✓ ").green(),
        FileState::Finished(Finished::Failed(_)) => Span::from("✗ ").red(),
        FileState::Finished(Finished::Exists(_)) => Span::from("! ").yellow(),
        FileState::Finished(Finished::Skipped) => Span::from("- ").dark_gray(),
    }
}

fn draw_progress(frame: &mut Frame, area: Rect, progress: Option<Progress>) {
    match progress {
        None => {}
        Some(Progress::Scan(progress)) => draw_scan_progress(frame, area, progress),
        Some(Progress::Transcription(progress)) => match progress.ratio() {
            Some(ratio) => frame.render_widget(gauge(ratio, progress.to_string()), area),
            None => frame.render_widget(Line::from(progress.to_string()), area),
        },
        Some(Progress::Separation(progress)) => match progress.ratio() {
            Some(ratio) => frame.render_widget(gauge(ratio, progress.to_string()), area),
            None => frame.render_widget(Line::from(progress.to_string()), area),
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
    fn display(&self) -> Span<'static> {
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
        .with_context(|| format!("failed to list {}", display::path(dir)))?
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

    fn running_on(files: &[&str], job: Job, run: RunId, cancel: CancellationToken) -> App {
        let (results_tx, _) = mpsc::unbounded_channel();
        let mut app = App::new(PathBuf::from("/videos"), vec![], results_tx);
        app.screen = Screen::Running {
            files: files
                .iter()
                .map(|&file| FileRun::waiting(file.into()))
                .collect(),
            job,
            run,
            cancel,
            cancelling: false,
        };
        app
    }

    fn running(job: Job, run: RunId, cancel: CancellationToken) -> App {
        running_on(&["/videos/stream.mkv"], job, run, cancel)
    }

    fn finish(app: &mut App, run: RunId, index: usize, finished: Finished) -> ControlFlow<()> {
        app.on_event(run, index, FileEvent::Finished(finished))
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn output(line: &str) -> Finished {
        Finished::Output(vec![Line::from(line.to_string())])
    }

    #[test]
    fn ignores_results_from_other_runs() {
        let mut app = running(Job::SyncCheck, RunId(1), CancellationToken::new());

        let _ = finish(&mut app, RunId(0), 0, output("stale"));

        assert!(matches!(app.screen, Screen::Running { run: RunId(1), .. }));
    }

    #[test]
    fn keeps_running_until_every_file_has_finished() {
        let mut app = running_on(
            &["/videos/a.mkv", "/videos/b.mkv"],
            Job::SyncCheck,
            RunId(0),
            CancellationToken::new(),
        );

        let _ = finish(&mut app, RunId(0), 1, output("b"));
        assert!(matches!(app.screen, Screen::Running { .. }));

        let _ = finish(&mut app, RunId(0), 0, output("a"));
        let Screen::Output { lines, .. } = &app.screen else {
            panic!("expected the output");
        };
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["a.mkv", "a", "", "b.mkv", "b"]
        );
    }

    #[test]
    fn shows_a_lone_file_without_its_name() {
        let mut app = running(Job::SyncCheck, RunId(0), CancellationToken::new());

        let _ = finish(&mut app, RunId(0), 0, output("clean"));

        let Screen::Output { lines, .. } = &app.screen else {
            panic!("expected the output");
        };
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["clean"]
        );
    }

    #[test]
    fn shows_what_finished_before_a_cancel() {
        let mut app = running_on(
            &["/videos/a.mkv", "/videos/b.mkv"],
            Job::Transcribe,
            RunId(0),
            CancellationToken::new(),
        );
        let _ = finish(&mut app, RunId(0), 0, output("done"));
        let _ = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));

        let _ = finish(&mut app, RunId(0), 1, Finished::Skipped);

        let Screen::Output { lines, .. } = &app.screen else {
            panic!("expected the output");
        };
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["a.mkv", "done", "", "b.mkv", "Skipped."]
        );
    }

    #[tokio::test]
    async fn skips_files_not_yet_started_once_cancelled() {
        let (results_tx, mut results_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        cancel.cancel();

        spawn_job(
            Job::Chapters,
            vec![(0, "/videos/a.mkv".into()), (1, "/videos/b.mkv".into())],
            false,
            RunId(0),
            cancel,
            results_tx,
        );

        let mut skipped = vec![];
        while let Some((_, index, event)) = results_rx.recv().await {
            assert!(matches!(event, FileEvent::Finished(Finished::Skipped)));
            skipped.push(index);
        }
        assert_eq!(skipped, [0, 1]);
    }

    #[test]
    fn esc_cancels_and_waits_for_the_tool_to_clean_up() {
        let cancel = CancellationToken::new();
        let mut app = running(Job::Transcribe, RunId(0), cancel.clone());

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
    fn returns_to_the_picker_once_a_cancelled_run_finishes() {
        let mut app = running(Job::Transcribe, RunId(0), CancellationToken::new());
        let _ = app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));

        let flow = finish(&mut app, RunId(0), 0, Finished::Skipped);

        assert!(flow.is_continue());
        assert!(matches!(
            app.screen,
            Screen::Pick {
                focus: Focus::Files
            }
        ));
    }

    #[test]
    fn ctrl_c_quits_once_the_running_tool_has_cleaned_up() {
        let cancel = CancellationToken::new();
        let mut app = running(Job::Transcribe, RunId(0), cancel.clone());

        let on_key = app.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let on_event = finish(&mut app, RunId(0), 0, Finished::Skipped);

        assert_eq!(
            (on_key, cancel.is_cancelled(), on_event),
            (ControlFlow::Continue(()), true, ControlFlow::Break(()))
        );
    }

    fn confirming(app: &mut App) {
        let _ = finish(
            app,
            RunId(0),
            0,
            Finished::Exists("/videos/stream.srt".into()),
        );
    }

    #[test]
    fn asks_before_overwriting_an_existing_output() {
        let mut app = running(Job::Transcribe, RunId(0), CancellationToken::new());

        confirming(&mut app);

        let Screen::ConfirmOverwrite { job, files } = &app.screen else {
            panic!("expected the overwrite confirmation");
        };
        assert_eq!(
            (*job, files[0].existing_output()),
            (Job::Transcribe, Some(Path::new("/videos/stream.srt")))
        );
    }

    /// A finished `a.mkv`, and a `b.mkv` that stopped before replacing `b.srt`.
    fn confirming_one_of_two() -> App {
        let mut app = running_on(
            &["/videos/a.mkv", "/videos/b.mkv"],
            Job::Transcribe,
            RunId(0),
            CancellationToken::new(),
        );
        let _ = finish(&mut app, RunId(0), 0, output("kept"));
        let _ = finish(
            &mut app,
            RunId(0),
            1,
            Finished::Exists("/videos/b.srt".into()),
        );
        app
    }

    #[tokio::test]
    async fn overwriting_reruns_only_the_stopped_files() {
        let mut app = confirming_one_of_two();

        let _ = app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE));

        let Screen::Running { files, .. } = &app.screen else {
            panic!("expected the rerun");
        };
        assert!(matches!(
            files[0].state,
            FileState::Finished(Finished::Output(_))
        ));
        assert!(matches!(files[1].state, FileState::Waiting));
    }

    #[test]
    fn declining_to_overwrite_keeps_the_other_results() {
        let mut app = confirming_one_of_two();

        let _ = app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));

        let Screen::Output { lines, .. } = &app.screen else {
            panic!("expected the output");
        };
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["a.mkv", "kept", "", "b.mkv", "Skipped."]
        );
    }

    #[tokio::test]
    async fn reruns_the_tool_once_overwriting_is_confirmed() {
        let mut app = running(Job::Transcribe, RunId(0), CancellationToken::new());
        confirming(&mut app);

        let _ = app.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(matches!(
            app.screen,
            Screen::Running {
                job: Job::Transcribe,
                ..
            }
        ));
    }

    #[test]
    fn returns_to_the_picker_when_overwriting_is_declined() {
        let mut app = running(Job::Transcribe, RunId(0), CancellationToken::new());
        confirming(&mut app);

        let _ = app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));

        assert!(matches!(app.screen, Screen::Pick { .. }));
    }

    fn idle(screen: Screen) -> App {
        let (results_tx, _) = mpsc::unbounded_channel();
        let files = vec![
            "/videos/a.mkv".into(),
            "/videos/b.mkv".into(),
            "/videos/c.mkv".into(),
        ];
        let mut app = App::new(PathBuf::from("/videos"), files, results_tx);
        app.screen = screen;
        app
    }

    fn picking_files() -> App {
        idle(Screen::Pick {
            focus: Focus::Files,
        })
    }

    fn running_files(app: &App) -> Vec<PathBuf> {
        let Screen::Running { files, .. } = &app.screen else {
            panic!("expected a run");
        };
        files.iter().map(|f| f.file.clone()).collect()
    }

    #[test]
    fn starts_on_the_tools_and_moves_to_the_files_on_enter() {
        let mut app = idle(Screen::Pick {
            focus: Focus::Tools,
        });

        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);

        assert!(matches!(
            app.screen,
            Screen::Pick {
                focus: Focus::Files
            }
        ));
        assert_eq!(app.tool_list.selected(), Some(1));
    }

    #[tokio::test]
    async fn runs_the_checked_files() {
        let mut app = picking_files();

        press(
            &mut app,
            &[
                KeyCode::Char(' '),
                KeyCode::Down,
                KeyCode::Down,
                KeyCode::Char(' '),
                KeyCode::Enter,
            ],
        );

        assert_eq!(
            running_files(&app),
            [PathBuf::from("/videos/a.mkv"), "/videos/c.mkv".into()]
        );
    }

    #[tokio::test]
    async fn runs_the_highlighted_file_when_none_are_checked() {
        let mut app = picking_files();

        press(&mut app, &[KeyCode::Down, KeyCode::Enter]);

        assert_eq!(running_files(&app), [PathBuf::from("/videos/b.mkv")]);
    }

    #[test]
    fn a_selects_all_then_clears() {
        let mut app = picking_files();

        press(&mut app, &[KeyCode::Char('a')]);
        assert_eq!(app.checked, BTreeSet::from([0, 1, 2]));

        press(&mut app, &[KeyCode::Char('a')]);
        assert_eq!(app.checked, BTreeSet::new());
    }

    #[tokio::test]
    async fn going_back_from_the_output_keeps_the_picks() {
        let mut app = picking_files();
        press(&mut app, &[KeyCode::Char(' '), KeyCode::Enter]);
        let _ = finish(&mut app, RunId(0), 0, output("done"));

        press(&mut app, &[KeyCode::Esc]);

        assert!(matches!(
            app.screen,
            Screen::Pick {
                focus: Focus::Files
            }
        ));
        assert_eq!(app.checked, BTreeSet::from([0]));
    }

    #[test]
    fn keeps_running_files_in_view_of_a_short_screen() {
        let files: Vec<FileRun> = ["a", "b", "c", "d"]
            .map(|name| {
                let mut run = FileRun::waiting(name.into());
                if name < "c" {
                    run.state = FileState::Finished(Finished::Skipped);
                }
                run
            })
            .into();

        assert_eq!(first_visible(&files, 1), 2);
        assert_eq!(first_visible(&files, 3), 1);
        assert_eq!(first_visible(&files, 10), 0);
    }

    fn press(app: &mut App, codes: &[KeyCode]) {
        for &code in codes {
            let _ = app.on_key(key(code, KeyModifiers::NONE));
        }
    }

    fn picking(tool: Tool, focus: Focus) -> App {
        let mut app = idle(Screen::Pick { focus });
        app.tool_list
            .select(Tool::ALL.iter().position(|&t| t == tool));
        app
    }

    fn focus(app: &App) -> Focus {
        let Screen::Pick { focus } = app.screen else {
            panic!("expected the picker");
        };
        focus
    }

    #[test]
    fn goes_from_a_tool_with_options_to_its_options_then_the_files() {
        let mut app = picking(Tool::DialogueSeparation, Focus::Tools);

        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(focus(&app), Focus::Options);

        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(focus(&app), Focus::Files);

        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(focus(&app), Focus::Options);

        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(focus(&app), Focus::Tools);
    }

    #[test]
    fn skips_the_options_for_a_tool_without_any() {
        let mut app = picking(Tool::Transcribe, Focus::Tools);

        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(focus(&app), Focus::Files);

        press(&mut app, &[KeyCode::Esc]);
        assert_eq!(focus(&app), Focus::Tools);
    }

    #[tokio::test]
    async fn runs_a_separation_with_the_chosen_options() {
        let mut app = picking(Tool::DialogueSeparation, Focus::Options);

        press(
            &mut app,
            &[
                KeyCode::Right,
                KeyCode::Down,
                KeyCode::Right,
                KeyCode::Enter,
                KeyCode::Enter,
            ],
        );

        let Screen::Running { job, .. } = &app.screen else {
            panic!("expected a run");
        };
        assert_eq!(*job, Job::Separate(Keep::Ambience, Backend::Sam));
    }

    #[test]
    fn offers_tiger_first_and_wraps_around_the_choices() {
        let mut app = picking(Tool::DialogueSeparation, Focus::Options);
        assert_eq!(app.backend, Backend::Tiger);

        press(&mut app, &[KeyCode::Down, KeyCode::Left]);
        assert_eq!(app.backend, Backend::Sam);

        press(&mut app, &[KeyCode::Right]);
        assert_eq!(app.backend, Backend::Tiger);
    }

    #[test]
    fn titles_a_separation_with_its_side_and_model() {
        assert_eq!(
            title(Job::Separate(Keep::Voice, Backend::Tiger)),
            "Remove noise (TIGER-DnR)"
        );
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
