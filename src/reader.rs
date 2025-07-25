//! Functions for reading data from stdin and passing to the parser. If stdin is a keyboard, it
//! supplies a killring, history, syntax highlighting, tab-completion and various other interactive
//! features.
//!
//! Internally the interactive mode functions rely in the functions of the input library to read
//! individual characters of input.
//!
//! Token search is handled incrementally. Actual searches are only done on when searching backwards,
//! since the previous results are saved. The last search position is remembered and a new search
//! continues from the last search position. All search results are saved in the list 'search_prev'.
//! When the user searches forward, i.e. presses Alt-down, the list is consulted for previous search
//! result, and subsequent backwards searches are also handled by consulting the list up until the
//! end of the list is reached, at which point regular searching will commence.
//!
//! In general interactive reads work with the tty protocols (CSI-U, etc) enabled; these are disabled
//! before calling out to fish script, wildcards, or completions. Note CSI-U protocol prevents
//! control-C from generating SIGINT, so failing to disable these would prevent cancellation of wildcard
//! expansion, etc.

use libc::{
    c_char, ECHO, EINTR, EIO, EISDIR, ENOTTY, EPERM, ESRCH, ICANON, ICRNL, IEXTEN, INLCR, IXOFF,
    IXON, ONLCR, OPOST, O_NONBLOCK, O_RDONLY, SIGINT, SIGTTIN, STDERR_FILENO, STDIN_FILENO,
    STDOUT_FILENO, TCSANOW, VMIN, VQUIT, VSUSP, VTIME, _POSIX_VDISABLE,
};
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use once_cell::sync::Lazy;
use once_cell::unsync::OnceCell;
#[cfg(not(target_has_atomic = "64"))]
use portable_atomic::AtomicU64;
use std::borrow::Cow;
use std::cell::RefCell;
use std::cell::RefMut;
use std::cell::UnsafeCell;
use std::cmp;
use std::io::BufReader;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::ops::Range;
use std::os::fd::BorrowedFd;
use std::os::fd::{AsRawFd, RawFd};
use std::pin::Pin;
use std::rc::Rc;
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use errno::{errno, Errno};

use crate::abbrs::abbrs_match;
use crate::ast::{self, is_same_node, Kind};
use crate::builtins::shared::ErrorCode;
use crate::builtins::shared::STATUS_CMD_ERROR;
use crate::builtins::shared::STATUS_CMD_OK;
use crate::common::{
    escape, escape_string, exit_without_destructors, get_ellipsis_char, get_obfuscation_read_char,
    restore_term_foreground_process_group_for_exit, shell_modes, str2wcstring, write_loop,
    EscapeFlags, EscapeStringStyle, ScopeGuard, PROGRAM_NAME, UTF8_BOM_WCHAR,
};
use crate::complete::{
    complete, complete_load, sort_and_prioritize, CompleteFlags, Completion, CompletionList,
    CompletionRequestOptions,
};
use crate::editable_line::{line_at_cursor, range_of_line_at_cursor, Edit, EditableLine};
use crate::env::EnvStack;
use crate::env::{EnvMode, Environment, Statuses};
use crate::exec::exec_subshell;
use crate::expand::expand_one;
use crate::expand::{expand_string, expand_tilde, ExpandFlags, ExpandResultCode};
use crate::fallback::fish_wcwidth;
use crate::fd_readable_set::poll_fd_readable;
use crate::fds::{make_fd_blocking, wopen_cloexec, AutoCloseFd};
use crate::flog::{FLOG, FLOGF};
#[allow(unused_imports)]
use crate::future::IsSomeAnd;
use crate::global_safety::RelaxedAtomicBool;
use crate::highlight::{
    autosuggest_validate_from_history, highlight_shell, parse_text_face_for_highlight,
    HighlightRole, HighlightSpec,
};
use crate::history::{
    history_session_id, in_private_mode, History, HistorySearch, PersistenceMode, SearchDirection,
    SearchType,
};
use crate::input::init_input;
use crate::input_common::{
    stop_query, CharEvent, CharInputStyle, CursorPositionQuery, ImplicitEvent, InputData,
    QueryResponseEvent, ReadlineCmd, TerminalQuery,
};
use crate::io::IoChain;
use crate::key::ViewportPosition;
use crate::kill::{kill_add, kill_replace, kill_yank, kill_yank_rotate};
use crate::libc::MB_CUR_MAX;
use crate::nix::{getpgrp, getpid, isatty};
use crate::operation_context::{get_bg_context, OperationContext};
use crate::pager::{PageRendering, Pager, SelectionMotion};
use crate::panic::AT_EXIT;
use crate::parse_constants::SourceRange;
use crate::parse_constants::{ParseTreeFlags, ParserTestErrorBits};
use crate::parse_util::parse_util_process_extent;
use crate::parse_util::MaybeParentheses;
use crate::parse_util::SPACES_PER_INDENT;
use crate::parse_util::{
    parse_util_cmdsubst_extent, parse_util_compute_indents, parse_util_contains_wildcards,
    parse_util_detect_errors, parse_util_escape_string_with_quote, parse_util_escape_wildcards,
    parse_util_get_line_from_offset, parse_util_get_offset, parse_util_get_offset_from_line,
    parse_util_lineno, parse_util_locate_cmdsubst_range, parse_util_token_extent,
};
use crate::parser::{BlockType, EvalRes, Parser};
use crate::proc::{
    have_proc_stat, hup_jobs, is_interactive_session, job_reap, jobs_requiring_warning_on_exit,
    print_exit_warning_for_jobs, proc_update_jiffies,
};
use crate::reader_history_search::{smartcase_flags, ReaderHistorySearch, SearchMode};
use crate::screen::is_dumb;
use crate::screen::{screen_force_clear_to_end, CharOffset, Screen};
use crate::should_flog;
use crate::signal::{
    signal_check_cancel, signal_clear_cancel, signal_reset_handlers, signal_set_handlers,
    signal_set_handlers_once,
};
use crate::terminal::BufferedOutputter;
use crate::terminal::Output;
use crate::terminal::Outputter;
use crate::terminal::TerminalCommand::{
    ClearScreen, DecrstAlternateScreenBuffer, DecrstMouseTracking, DecsetAlternateScreenBuffer,
    DecsetShowCursor, Osc0WindowTitle, Osc133CommandFinished, Osc133CommandStart,
    QueryCursorPosition, QueryKittyKeyboardProgressiveEnhancements, QueryPrimaryDeviceAttribute,
    QueryXtgettcap, QueryXtversion,
};
use crate::terminal::{Capability, SCROLL_FORWARD_SUPPORTED, SCROLL_FORWARD_TERMINFO_CODE};
use crate::termsize::{termsize_invalidate_tty, termsize_last, termsize_update};
use crate::text_face::parse_text_face;
use crate::text_face::TextFace;
use crate::threads::{
    assert_is_background_thread, assert_is_main_thread, iothread_service_main_with_timeout,
    Debounce,
};
use crate::tokenizer::quote_end;
use crate::tokenizer::variable_assignment_equals_pos;
use crate::tokenizer::{
    tok_command, MoveWordStateMachine, MoveWordStyle, TokenType, Tokenizer, TOK_ACCEPT_UNFINISHED,
    TOK_SHOW_COMMENTS,
};
use crate::tty_handoff::{
    get_kitty_keyboard_capability, get_tty_protocols_active, initialize_tty_metadata,
    safe_deactivate_tty_protocols, set_kitty_keyboard_capability, tty_metadata, TtyHandoff,
};
use crate::wchar::prelude::*;
use crate::wcstringutil::string_prefixes_string_maybe_case_insensitive;
use crate::wcstringutil::{
    count_preceding_backslashes, join_strings, string_prefixes_string,
    string_prefixes_string_case_insensitive, StringFuzzyMatch,
};
use crate::wildcard::wildcard_has;
use crate::wutil::{fstat, perror, write_to_fd, wstat};
use crate::{abbrs, event, function};

/// A description of where fish is in the process of exiting.
#[repr(u8)]
enum ExitState {
    /// fish is not exiting.
    None,
    /// fish intends to exit, and is running handlers like 'fish_exit'.
    RunningHandlers,
    /// fish is finished running handlers and no more fish script may be run.
    FinishedHandlers,
}

static EXIT_STATE: AtomicU8 = AtomicU8::new(ExitState::None as u8);

pub static SHELL_MODES: Lazy<Mutex<libc::termios>> =
    Lazy::new(|| Mutex::new(unsafe { std::mem::zeroed() }));

/// The valid terminal modes on startup.
/// Warning: this is read from the SIGTERM handler! Hence the raw global.
static TERMINAL_MODE_ON_STARTUP: once_cell::sync::OnceCell<libc::termios> =
    once_cell::sync::OnceCell::new();

/// Mode we use to execute programs.
static TTY_MODES_FOR_EXTERNAL_CMDS: Lazy<Mutex<libc::termios>> =
    Lazy::new(|| Mutex::new(unsafe { std::mem::zeroed() }));

static RUN_COUNT: AtomicU64 = AtomicU64::new(0);

static STATUS_COUNT: AtomicU64 = AtomicU64::new(0);

/// This variable is set to a signal by the signal handler when ^C is pressed.
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);

/// If set, SIGHUP has been received. This latches to true.
/// This is set from a signal handler.
static SIGHUP_RECEIVED: RelaxedAtomicBool = RelaxedAtomicBool::new(false);

// Get the terminal mode on startup. This is "safe" because it's async-signal safe.
pub fn safe_get_terminal_mode_on_startup() -> Option<&'static libc::termios> {
    TERMINAL_MODE_ON_STARTUP.get()
}

/// A singleton snapshot of the reader state. This is factored out for thread-safety reasons:
/// it may be fetched on a background thread.
fn commandline_state_snapshot() -> MutexGuard<'static, CommandlineState> {
    static STATE: Mutex<CommandlineState> = Mutex::new(CommandlineState::new());
    STATE.lock().unwrap()
}

/// Any time the contents of a buffer changes, we update the generation count. This allows for our
/// background threads to notice it and skip doing work that they would otherwise have to do.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Get the debouncer for autosuggestions and background highlighting.
fn debounce_autosuggestions() -> &'static Debounce {
    const AUTOSUGGEST_TIMEOUT: Duration = Duration::from_millis(500);
    static RES: once_cell::race::OnceBox<Debounce> = once_cell::race::OnceBox::new();
    RES.get_or_init(|| Box::new(Debounce::new(AUTOSUGGEST_TIMEOUT)))
}

fn debounce_highlighting() -> &'static Debounce {
    const HIGHLIGHT_TIMEOUT: Duration = Duration::from_millis(500);
    static RES: once_cell::race::OnceBox<Debounce> = once_cell::race::OnceBox::new();
    RES.get_or_init(|| Box::new(Debounce::new(HIGHLIGHT_TIMEOUT)))
}

fn debounce_history_pager() -> &'static Debounce {
    const HISTORY_PAGER_TIMEOUT: Duration = Duration::from_millis(500);
    static RES: once_cell::race::OnceBox<Debounce> = once_cell::race::OnceBox::new();
    RES.get_or_init(|| Box::new(Debounce::new(HISTORY_PAGER_TIMEOUT)))
}

fn redirect_tty_after_sighup() {
    use std::fs::OpenOptions;

    // If we have received SIGHUP, redirect the tty to avoid a user script triggering SIGTTIN or
    // SIGTTOU.
    assert!(reader_received_sighup(), "SIGHUP not received");
    static TTY_REDIRECTED: RelaxedAtomicBool = RelaxedAtomicBool::new(false);
    if TTY_REDIRECTED.swap(true) {
        return;
    }
    // dup2 all ENOTTY / EIOs to /dev/null.
    let Ok(devnull) = OpenOptions::new().read(true).write(true).open("/dev/null") else {
        return;
    };
    let fd = devnull.as_raw_fd();
    for stdfd in [STDIN_FILENO, STDOUT_FILENO, STDERR_FILENO] {
        let mut t = std::mem::MaybeUninit::uninit();
        unsafe {
            if libc::tcgetattr(stdfd, t.as_mut_ptr()) != 0
                && matches!(errno::errno().0, EIO | ENOTTY)
            {
                libc::dup2(fd, stdfd);
            }
        }
    }
}

pub(crate) fn initial_query(
    blocking_query: &OnceCell<RefCell<Option<TerminalQuery>>>,
    out: &mut impl Output,
    vars: Option<&dyn Environment>,
) {
    blocking_query.get_or_init(|| {
        let md = tty_metadata();
        let query = if is_dumb() || md.in_midnight_commander || md.in_dvtm || !isatty(STDOUT_FILENO)
        {
            None
        } else {
            // Query for kitty keyboard protocol support.
            out.write_command(QueryKittyKeyboardProgressiveEnhancements);
            out.write_command(QueryXtversion);
            if let Some(vars) = vars {
                query_capabilities_via_dcs(out.by_ref(), vars);
            }
            out.write_command(QueryPrimaryDeviceAttribute);
            Some(TerminalQuery::PrimaryDeviceAttribute)
        };
        RefCell::new(query)
    });
}

/// The stack of current interactive reading contexts.
fn reader_data_stack() -> &'static mut Vec<Pin<Box<ReaderData>>> {
    struct ReaderDataStack(UnsafeCell<Vec<Pin<Box<ReaderData>>>>);
    // Safety: only used on main thread.
    unsafe impl Sync for ReaderDataStack {}

    static READER_DATA_STACK: ReaderDataStack = ReaderDataStack(UnsafeCell::new(vec![]));

    assert_is_main_thread();
    unsafe { &mut *READER_DATA_STACK.0.get() }
}

pub fn reader_in_interactive_read() -> bool {
    reader_data_stack()
        .iter()
        .rev()
        .any(|reader| reader.conf.exit_on_interrupt)
}

/// Access the top level reader data.
pub fn current_data() -> Option<&'static mut ReaderData> {
    reader_data_stack()
        .last_mut()
        .map(|data| unsafe { Pin::get_unchecked_mut(Pin::as_mut(data)) })
}
pub use current_data as reader_current_data;

/// Add a new reader to the reader stack.
/// If `history_name` is empty, then save history in-memory only; do not write it to disk.
pub fn reader_push<'a>(parser: &'a Parser, history_name: &wstr, conf: ReaderConfig) -> Reader<'a> {
    assert_is_main_thread();
    let hist = History::with_name(history_name);
    hist.resolve_pending();
    let is_top_level = reader_data_stack().is_empty();
    let data = ReaderData::new(hist, conf, is_top_level);
    reader_data_stack().push(data);
    let data = current_data().unwrap();
    data.command_line_changed(EditableLineTag::Commandline, AutosuggestionUpdate::Remove);
    if is_top_level {
        reader_interactive_init(parser);
    }
    Reader { data, parser }
}

/// Return to previous reader environment.
pub fn reader_pop() {
    assert_is_main_thread();
    reader_data_stack().pop().unwrap();
    if let Some(new_reader) = current_data() {
        new_reader
            .screen
            .reset_abandoning_line(usize::try_from(termsize_last().width).unwrap());
    } else {
        reader_interactive_destroy();
        *commandline_state_snapshot() = CommandlineState::new();
    }
}

/// Configuration that we provide to a reader.
#[derive(Default)]
pub struct ReaderConfig {
    /// Left prompt command, typically fish_prompt.
    pub left_prompt_cmd: WString,

    /// Right prompt command, typically fish_right_prompt.
    pub right_prompt_cmd: WString,

    /// Name of the event to trigger once we're set up.
    pub event: &'static wstr,

    /// Whether tab completion is OK.
    pub complete_ok: bool,

    /// Whether to perform syntax highlighting.
    pub highlight_ok: bool,

    /// Whether to perform syntax checking before returning.
    pub syntax_check_ok: bool,

    /// Whether to allow autosuggestions.
    pub autosuggest_ok: bool,

    /// Whether to reexecute prompt function before final rendering.
    pub transient_prompt: bool,

    /// Whether to expand abbreviations.
    pub expand_abbrev_ok: bool,

    /// Whether to exit on interrupt (^C).
    pub exit_on_interrupt: bool,

    /// If set, do not show what is typed.
    pub in_silent_mode: bool,

    /// The fd for stdin, default to actual stdin.
    pub inputfd: RawFd,
}

/// Snapshotted state from the reader.
#[derive(Clone, Default)]
pub struct CommandlineState {
    /// command line text, or empty if not interactive
    pub text: WString,
    /// position of the cursor, may be as large as text.size()
    pub cursor_pos: usize,
    /// visual selection, or none if none
    pub selection: Option<Range<usize>>,
    /// current reader history, or null if not interactive
    pub history: Option<Arc<History>>,
    /// pager is visible
    pub pager_mode: bool,
    /// pager already shows everything if possible
    pub pager_fully_disclosed: bool,
    /// The search field, if shown.
    pub search_field: Option<(WString, usize)>,
    /// pager is visible and search is active
    pub search_mode: bool,
}

impl CommandlineState {
    const fn new() -> Self {
        Self {
            text: WString::new(),
            cursor_pos: 0,
            selection: None,
            history: None,
            pager_mode: false,
            pager_fully_disclosed: false,
            search_field: None,
            search_mode: false,
        }
    }
}

/// Strategy for determining how the selection behaves.
#[derive(Eq, PartialEq)]
pub enum CursorSelectionMode {
    /// The character at/after the cursor is excluded.
    /// This is most useful with a line cursor shape.
    Exclusive,
    /// The character at/after the cursor is included.
    /// This is most useful with a block or underscore cursor shape.
    Inclusive,
}

#[derive(Eq, PartialEq)]
pub enum CursorEndMode {
    Exclusive,
    Inclusive,
}

/// A mode for calling the reader_kill function.
enum Kill {
    /// In this mode, the new string is appended to the current contents of the kill buffer.
    Append,
    /// In this mode, the new string is prepended to the current contents of the kill buffer.
    Prepend,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JumpDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JumpPrecision {
    Till,
    To,
}

/// readline_loop_state_t encapsulates the state used in a readline loop.
struct ReadlineLoopState {
    /// The last command that was executed.
    last_cmd: Option<ReadlineCmd>,

    /// If the last command was a yank, the length of yanking that occurred.
    yank_len: usize,

    /// If the last "complete" readline command has inserted text into the command line.
    complete_did_insert: bool,

    /// List of completions.
    comp: Vec<Completion>,

    /// Whether the loop has finished, due to reaching the character limit or through executing a
    /// command.
    finished: bool,

    /// Maximum number of characters to read.
    nchars: Option<NonZeroUsize>,
}

impl ReadlineLoopState {
    fn new() -> Self {
        Self {
            last_cmd: None,
            yank_len: 0,
            complete_did_insert: true,
            comp: vec![],
            finished: false,
            nchars: None,
        }
    }
}

/// Data wrapping up the visual selection.
#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct SelectionData {
    /// The position of the cursor when selection was initiated.
    begin: usize,

    /// The start and stop position of the current selection.
    start: usize,
    stop: usize,
}

/// A value-type struct representing a layout that can be rendered.
/// The intent is that everything we send to the screen is encapsulated in this struct.
#[derive(Clone, Default)]
struct LayoutData {
    /// Text of the command line.
    text: WString,

    /// The colors. This has the same length as 'text'.
    colors: Vec<HighlightSpec>,

    /// Position of the cursor in the command line.
    position: usize,

    /// The cursor position in the pager search field.
    pager_search_field_position: Option<usize>,

    /// Visual selection of the command line, or none if none.
    selection: Option<SelectionData>,

    /// String containing the autosuggestion.
    autosuggestion: WString,

    /// The matching range of the command line from a history search. If non-empty, then highlight
    /// the range within the text.
    history_search_range: Option<SourceRange>,

    /// The result of evaluating the left, mode and right prompt commands.
    /// That is, this the text of the prompts, not the commands to produce them.
    left_prompt_buff: WString,
    mode_prompt_buff: WString,
    right_prompt_buff: WString,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum EditableLineTag {
    Commandline,
    SearchField,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TransientEdit {
    Pager,
    HistorySearch,
}

/// A struct describing the state of the interactive reader. These states can be stacked, in case
/// reader_readline() calls are nested. This happens when the 'read' builtin is used.
/// ReaderData does not contain a Parser - by itself it cannot execute fish script.
pub struct ReaderData {
    /// We could put the entire thing in an Rc but Rc::get_unchecked_mut is not yet stable.
    /// This is sufficient for our use.
    canary: Rc<()>,
    /// Configuration for the reader.
    conf: ReaderConfig,
    /// String containing the whole current commandline.
    command_line: EditableLine,
    /// Whether the most recent modification to the command line was done by either history search
    /// or a pager selection change. When this is true and another transient change is made, the
    /// old transient change will be removed from the undo history.
    command_line_transient_edit: Option<TransientEdit>,
    /// The most recent layout data sent to the screen.
    rendered_layout: LayoutData,
    /// The current autosuggestion.
    autosuggestion: Autosuggestion,
    /// A previously valid autosuggestion.
    saved_autosuggestion: Option<Autosuggestion>,
    /// Current pager.
    pager: Pager,
    /// The output of the pager.
    current_page_rendering: PageRendering,
    /// When backspacing, we temporarily suppress autosuggestions.
    suppress_autosuggestion: bool,

    /// HACK: A flag to reset the loop state from the outside.
    reset_loop_state: bool,

    /// Whether this is the first prompt.
    first_prompt: bool,

    /// The time when the last flash() completed.
    last_flash: Option<Instant>,

    /// Whether flash autosuggestion.
    flash_autosuggestion: bool,

    /// The representation of the current screen contents.
    screen: Screen,

    /// Data associated with input events.
    /// This is made public so that InputEventQueuer can be implemented on us.
    pub input_data: InputData,
    queued_repaint: bool,
    /// The history.
    history: Arc<History>,
    /// The history search.
    history_search: ReaderHistorySearch,
    /// In-pager history search.
    history_pager: Option<Range<usize>>,

    /// The cursor selection mode.
    cursor_selection_mode: CursorSelectionMode,
    cursor_end_mode: CursorEndMode,

    /// The selection data. If this is not none, then we have an active selection.
    selection: Option<SelectionData>,

    left_prompt_buff: WString,
    mode_prompt_buff: WString,
    /// The output of the last evaluation of the right prompt command.
    right_prompt_buff: WString,

    /// When navigating the pager, we modify the command line.
    /// This is the saved command line before modification.
    cycle_command_line: WString,
    cycle_cursor_pos: usize,

    /// If set, a key binding or the 'exit' command has asked us to exit our read loop.
    exit_loop_requested: bool,
    /// If this is true, exit reader even if there are running jobs. This happens if we press e.g.
    /// ^D twice.
    did_warn_for_bg_jobs: bool,
    /// The current contents of the top item in the kill ring.
    kill_item: WString,

    /// A flag which may be set to force re-execing all prompts and re-rendering.
    /// This may come about when a color like $fish_color... has changed.
    force_exec_prompt_and_repaint: bool,

    /// The target character of the last jump command.
    last_jump_target: Option<char>,
    last_jump_direction: JumpDirection,
    last_jump_precision: JumpPrecision,

    /// The text of the most recent asynchronous highlight and autosuggestion requests.
    /// If these differs from the text of the command line, then we must kick off a new request.
    in_flight_highlight_request: WString,
    in_flight_autosuggest_request: WString,

    rls: Option<ReadlineLoopState>,
}

/// Reader is ReaderData equippeed with a Parser, so it can execute fish script.
pub struct Reader<'a> {
    pub data: &'a mut ReaderData,
    pub parser: &'a Parser,
}

/// Reader dereferences to its referenced ReaderData.
impl<'a> std::ops::Deref for Reader<'a> {
    type Target = ReaderData;
    fn deref(&self) -> &ReaderData {
        self.data
    }
}

impl<'a> std::ops::DerefMut for Reader<'a> {
    fn deref_mut(&mut self) -> &mut ReaderData {
        self.data
    }
}

impl<'a> Reader<'a> {
    /// Return the variable set used for e.g. command duration.
    fn vars(&self) -> &dyn Environment {
        self.parser.vars()
    }
}

/// Read commands from \c fd until encountering EOF.
/// The fd is not closed.
pub fn reader_read(parser: &Parser, fd: RawFd, io: &IoChain) -> Result<(), ErrorCode> {
    // If reader_read is called recursively through the '.' builtin, we need to preserve
    // is_interactive. This, and signal handler setup is handled by
    // proc_push_interactive/proc_pop_interactive.
    let interactive = (fd == STDIN_FILENO) && isatty(STDIN_FILENO);

    let _interactive_push = parser.push_scope(|s| s.is_interactive = interactive);
    signal_set_handlers_once(interactive);

    let res = if interactive {
        read_i(parser);
        Ok(())
    } else {
        read_ni(parser, fd, io)
    };

    // If the exit command was called in a script, only exit the script, not the program.
    parser.libdata_mut().exit_current_script = false;

    res
}

/// Read interactively. Read input from stdin while providing editing facilities.
fn read_i(parser: &Parser) {
    assert_is_main_thread();
    let mut conf = ReaderConfig::default();
    conf.event = L!("fish_prompt");
    conf.complete_ok = true;
    conf.highlight_ok = true;
    conf.syntax_check_ok = true;
    conf.expand_abbrev_ok = true;
    conf.autosuggest_ok = check_bool_var(parser.vars(), L!("fish_autosuggestion_enabled"), true);
    conf.transient_prompt = check_bool_var(parser.vars(), L!("fish_transient_prompt"), false);

    if parser.is_breakpoint() && function::exists(DEBUG_PROMPT_FUNCTION_NAME, parser) {
        conf.left_prompt_cmd = DEBUG_PROMPT_FUNCTION_NAME.to_owned();
        conf.right_prompt_cmd.clear();
    } else {
        conf.left_prompt_cmd = LEFT_PROMPT_FUNCTION_NAME.to_owned();
        conf.right_prompt_cmd = RIGHT_PROMPT_FUNCTION_NAME.to_owned();
    }

    let mut data = reader_push(parser, &history_session_id(parser.vars()), conf);
    data.import_history_if_necessary();

    // Set up tty protocols. These should be enabled while we're reading interactively,
    // and disabled before we run fish script, wildcards, or completions. This is scoped.
    // Note this may be disabled within the loop, e.g. when running fish script bound to keys.
    let mut tty = TtyHandoff::new();

    while !check_exit_loop_maybe_warning(Some(&mut data)) {
        RUN_COUNT.fetch_add(1, Ordering::Relaxed);

        let Some(command) = data.readline(None) else {
            continue;
        };

        if command.is_empty() {
            continue;
        }

        // Got a command. Disable tty protocols while we execute it.
        tty.disable_tty_protocols();
        data.clear(EditableLineTag::Commandline);
        data.update_buff_pos(EditableLineTag::Commandline, None);
        BufferedOutputter::new(Outputter::stdoutput()).write_command(Osc133CommandStart(&command));
        event::fire_generic(parser, L!("fish_preexec").to_owned(), vec![command.clone()]);
        let eval_res = reader_run_command(parser, &command);
        signal_clear_cancel();
        if !eval_res.no_status {
            STATUS_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // If the command requested an exit, then process it now and clear it.
        data.exit_loop_requested |= parser.libdata().exit_current_script;
        parser.libdata_mut().exit_current_script = false;

        BufferedOutputter::new(Outputter::stdoutput())
            .write_command(Osc133CommandFinished(parser.get_last_status()));
        event::fire_generic(parser, L!("fish_postexec").to_owned(), vec![command]);
        // Allow any pending history items to be returned in the history array.
        data.history.resolve_pending();

        // Make cursor visible. Every even vaguely used terminal agrees on this sequence.
        data.screen.write_command(DecsetShowCursor);

        let already_warned = data.did_warn_for_bg_jobs;
        if check_exit_loop_maybe_warning(Some(&mut data)) {
            break;
        }
        if already_warned {
            // We had previously warned the user and they ran another command.
            // Reset the warning.
            data.did_warn_for_bg_jobs = false;
        }
    }
    reader_pop();

    // If we got SIGHUP, ensure the tty is redirected and release tty handoff without
    // trying to muck with protocols.
    if reader_received_sighup() {
        // If we are the top-level reader, then we translate SIGHUP into exit_forced.
        redirect_tty_after_sighup();
    }

    // If we are the last reader, then kill remaining jobs before exiting.
    if reader_data_stack().is_empty() {
        // Send the exit event and then commit to not executing any more fish script.
        EXIT_STATE.store(ExitState::RunningHandlers as u8, Ordering::Relaxed);
        event::fire_generic(parser, L!("fish_exit").to_owned(), vec![]);
        EXIT_STATE.store(ExitState::FinishedHandlers as u8, Ordering::Relaxed);
        hup_jobs(&parser.jobs());
    }
}

/// Read non-interactively.  Read input from stdin without displaying the prompt, using syntax
/// highlighting. This is used for reading scripts and init files.
/// The file is not closed.
fn read_ni(parser: &Parser, fd: RawFd, io: &IoChain) -> Result<(), ErrorCode> {
    let md = match fstat(fd) {
        Ok(md) => md,
        Err(err) => {
            FLOG!(
                error,
                wgettext_fmt!("Unable to read input file: %s", err.to_string())
            );
            return Err(STATUS_CMD_ERROR);
        }
    };

    /* FreeBSD allows read() on directories. Error explicitly in that case. */
    // XXX: This can be triggered spuriously, so we'll not do that for stdin.
    // This can be seen e.g. with node's "spawn" api.
    if fd != STDIN_FILENO && md.is_dir() {
        FLOG!(
            error,
            wgettext_fmt!("Unable to read input file: %s", Errno(EISDIR).to_string())
        );
        return Err(STATUS_CMD_ERROR);
    }

    // Read all data into a vec.
    let mut fd_contents = Vec::with_capacity(usize::try_from(md.len()).unwrap());
    loop {
        let mut buff = [0_u8; 4096];

        match nix::unistd::read(unsafe { BorrowedFd::borrow_raw(fd) }, &mut buff) {
            Ok(0) => {
                // EOF.
                break;
            }
            Ok(amt) => {
                fd_contents.extend_from_slice(&buff[..amt]);
            }
            Err(err) => {
                if err == nix::Error::EINTR {
                    continue;
                } else if (err == nix::Error::EAGAIN || err == nix::Error::EWOULDBLOCK)
                    && make_fd_blocking(fd).is_ok()
                {
                    // We succeeded in making the fd blocking, keep going.
                    continue;
                } else {
                    // Fatal error.
                    FLOG!(
                        error,
                        wgettext_fmt!("Unable to read input file: %s", err.to_string())
                    );
                    return Err(STATUS_CMD_ERROR);
                }
            }
        }
    }

    let mut s = str2wcstring(&fd_contents);

    // Eagerly deallocate to save memory.
    drop(fd_contents);

    // Swallow a BOM (issue #1518).
    if s.chars().next() == Some(UTF8_BOM_WCHAR) {
        s.remove(0);
    }

    match parser.eval_wstr(s, io, None, BlockType::top) {
        Ok(_) => Ok(()),
        Err(msg) => {
            eprintf!("%ls", msg);
            Err(STATUS_CMD_ERROR)
        }
    }
}

/// Initialize the reader.
pub fn reader_init(will_restore_foreground_pgroup: bool) {
    // Save the initial terminal mode.
    // Note this field is read by a signal handler, so do it atomically, with a leaked mode.
    let mut terminal_mode_on_startup = unsafe { std::mem::zeroed::<libc::termios>() };
    let ret = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut terminal_mode_on_startup) };
    // TODO: rationalize behavior if initial tcgetattr() fails.
    if ret == 0 {
        TERMINAL_MODE_ON_STARTUP.get_or_init(|| terminal_mode_on_startup);
    }

    #[cfg(not(test))]
    assert!(AT_EXIT.get().is_none());
    AT_EXIT.get_or_init(|| Box::new(move || reader_deinit(will_restore_foreground_pgroup)));

    // Set the mode used for program execution, initialized to the current mode.
    let mut tty_modes_for_external_cmds = TTY_MODES_FOR_EXTERNAL_CMDS.lock().unwrap();
    *tty_modes_for_external_cmds = terminal_mode_on_startup;
    term_fix_external_modes(&mut tty_modes_for_external_cmds);

    // Disable flow control by default.
    tty_modes_for_external_cmds.c_iflag &= !IXON;
    tty_modes_for_external_cmds.c_iflag &= !IXOFF;

    // Set the mode used for the terminal, initialized to the current mode.
    *shell_modes() = *tty_modes_for_external_cmds;

    term_fix_modes(&mut shell_modes());

    drop(tty_modes_for_external_cmds);

    // Set up our fixed terminal modes once,
    // so we don't get flow control just because we inherited it.
    if is_interactive_session() {
        if getpgrp() == unsafe { libc::tcgetpgrp(STDIN_FILENO) } {
            term_donate(/*quiet=*/ true);
        }
    }
}

pub fn reader_deinit(restore_foreground_pgroup: bool) {
    safe_restore_term_mode();
    safe_deactivate_tty_protocols();
    if restore_foreground_pgroup {
        restore_term_foreground_process_group_for_exit();
    }
}

/// Restore the term mode if we own the terminal and are interactive (#8705).
/// It's important we do this before restore_foreground_process_group,
/// otherwise we won't think we own the terminal.
/// THIS FUNCTION IS CALLED FROM A SIGNAL HANDLER. IT MUST BE ASYNC-SIGNAL-SAFE.
pub fn safe_restore_term_mode() {
    if !is_interactive_session() || getpgrp() != unsafe { libc::tcgetpgrp(STDIN_FILENO) } {
        return;
    }
    if let Some(modes) = safe_get_terminal_mode_on_startup() {
        unsafe { libc::tcsetattr(STDIN_FILENO, TCSANOW, modes) };
    }
}

/// Change the history file for the current command reading context.
pub fn reader_change_history(name: &wstr) {
    // We don't need to _change_ if we're not initialized yet.
    let Some(data) = current_data() else {
        return;
    };

    data.history.save();
    data.history = History::with_name(name);
    commandline_state_snapshot().history = Some(data.history.clone());
}

pub fn reader_change_cursor_selection_mode(selection_mode: CursorSelectionMode) {
    // We don't need to _change_ if we're not initialized yet.
    if let Some(data) = current_data() {
        if data.cursor_selection_mode == selection_mode {
            return;
        }
        let invalidates_selection = data.selection.is_some();
        data.cursor_selection_mode = selection_mode;
        if invalidates_selection {
            data.update_buff_pos(EditableLineTag::Commandline, None);
        }
    }
}

pub fn reader_change_cursor_end_mode(end_mode: CursorEndMode) {
    // We don't need to _change_ if we're not initialized yet.
    if let Some(data) = current_data() {
        if data.cursor_end_mode == end_mode {
            return;
        }
        let invalidates_end = data.selection.is_some();
        data.cursor_end_mode = end_mode;
        if invalidates_end {
            data.update_buff_pos(EditableLineTag::Commandline, None);
        }
    }
}

fn check_bool_var(vars: &dyn Environment, name: &wstr, default: bool) -> bool {
    vars.get(name)
        .map(|v| v.as_string())
        .map(|v| v != L!("0"))
        .unwrap_or(default)
}

/// Enable or disable autosuggestions based on the associated variable.
pub fn reader_set_autosuggestion_enabled(vars: &dyn Environment) {
    // We don't need to _change_ if we're not initialized yet.
    if let Some(data) = current_data() {
        let enable = check_bool_var(vars, L!("fish_autosuggestion_enabled"), true);
        if data.conf.autosuggest_ok != enable {
            data.conf.autosuggest_ok = enable;
            data.force_exec_prompt_and_repaint = true;
            data.input_data
                .queue_char(CharEvent::from_readline(ReadlineCmd::Repaint));
        }
    }
}

/// Enable or disable transient prompt based on the associated variable.
pub fn reader_set_transient_prompt(vars: &dyn Environment) {
    // We don't need to _change_ if we're not initialized yet.
    if let Some(data) = current_data() {
        data.conf.transient_prompt = check_bool_var(vars, L!("fish_transient_prompt"), false);
    }
}

/// Tell the reader that it needs to re-exec the prompt and repaint.
/// This may be called in response to e.g. a color variable change.
pub fn reader_schedule_prompt_repaint() {
    assert_is_main_thread();
    let Some(data) = current_data() else {
        return;
    };
    if !data.force_exec_prompt_and_repaint {
        data.force_exec_prompt_and_repaint = true;
        data.input_data
            .queue_char(CharEvent::from_readline(ReadlineCmd::Repaint));
    }
}

pub fn reader_execute_readline_cmd(parser: &Parser, ch: CharEvent) {
    if parser.scope().readonly_commandline {
        return;
    }
    if let Some(data) = current_data() {
        let mut data = Reader { parser, data };
        let CharEvent::Readline(readline_cmd_evt) = &ch else {
            panic!()
        };
        if matches!(
            readline_cmd_evt.cmd,
            ReadlineCmd::ClearScreenAndRepaint
                | ReadlineCmd::RepaintMode
                | ReadlineCmd::Repaint
                | ReadlineCmd::ForceRepaint
        ) {
            data.queued_repaint = true;
        }
        if data.queued_repaint {
            data.input_data.queue_char(ch);
            return;
        }
        if data.rls.is_none() {
            data.rls = Some(ReadlineLoopState::new());
        }
        data.save_screen_state();
        let _ = data.handle_char_event(Some(ch));
    }
}

pub fn reader_showing_suggestion(parser: &Parser) -> bool {
    if !is_interactive_session() {
        return false;
    }
    if let Some(data) = current_data() {
        let reader = Reader { parser, data };
        let suggestion = &reader.autosuggestion.text;
        let is_single_space = suggestion.ends_with(L!(" "))
            && line_at_cursor(reader.command_line.text(), reader.command_line.position())
                == suggestion[..suggestion.len() - 1];
        !suggestion.is_empty() && !is_single_space
    } else {
        false
    }
}

/// Return the value of the interrupted flag, which is set by the sigint handler, and clear it if it
/// was set. If the current reader is interruptible, mark the reader as exit_loop_requested.
pub fn reader_reading_interrupted(data: &mut ReaderData) -> i32 {
    let res = reader_test_and_clear_interrupted();
    if res == 0 {
        return 0;
    }
    if data.conf.exit_on_interrupt {
        data.exit_loop_requested = true;
        // We handled the interrupt ourselves, our caller doesn't need to handle it.
        return 0;
    }
    res
}

/// Read one line of input. Before calling this function, reader_push() must have been called in
/// order to set up a valid reader environment. If nchars is given, return after reading that many
/// characters even if a full line has not yet been read. Note: the returned value may be longer
/// than nchars if a single keypress resulted in multiple characters being inserted into the
/// commandline.
pub fn reader_readline(parser: &Parser, nchars: Option<NonZeroUsize>) -> Option<WString> {
    let data = current_data().unwrap();
    let mut reader = Reader { parser, data };
    reader.readline(nchars)
}

/// Get the command line state. This may be fetched on a background thread.
pub fn commandline_get_state(sync: bool) -> CommandlineState {
    if sync {
        current_data().map(|data| data.update_commandline_state());
    }
    commandline_state_snapshot().clone()
}

/// Set the command line text and position. This may be called on a background thread; the reader
/// will pick it up when it is done executing.
pub fn commandline_set_buffer(parser: &Parser, text: Option<WString>, cursor_pos: Option<usize>) {
    if parser.scope().readonly_commandline {
        return;
    }
    {
        let mut state = commandline_state_snapshot();
        if let Some(text) = text {
            state.text = text;
        }
        state.cursor_pos = cmp::min(cursor_pos.unwrap_or(usize::MAX), state.text.len());
    }
    current_data().map(|data| data.apply_commandline_state_changes());
}

pub fn commandline_set_search_field(parser: &Parser, text: WString, cursor_pos: Option<usize>) {
    if parser.scope().readonly_commandline {
        return;
    }
    {
        let mut state = commandline_state_snapshot();
        assert!(state.search_field.is_some());
        let new_pos = cmp::min(cursor_pos.unwrap_or(usize::MAX), text.len());
        state.search_field = Some((text, new_pos));
    }
    current_data().map(|data| data.apply_commandline_state_changes());
}

/// Return the current interactive reads loop count. Useful for determining how many commands have
/// been executed between invocations of code.
pub fn reader_run_count() -> u64 {
    RUN_COUNT.load(Ordering::Relaxed)
}

/// Returns the current "generation" of interactive status. Useful for determining whether the
/// previous command produced a status.
/// This is not incremented if the command being run produces no status,
/// (e.g. background job, or variable assignment).
pub fn reader_status_count() -> u64 {
    STATUS_COUNT.load(Ordering::Relaxed)
}

// Name of the variable that tells how long it took, in milliseconds, for the previous
// interactive command to complete.
const ENV_CMD_DURATION: &wstr = L!("CMD_DURATION");

/// Maximum length of prefix string when printing completion list. Longer prefixes will be
/// ellipsized.
const PREFIX_MAX_LEN: usize = 9;

/// A simple prompt for reading shell commands that does not rely on fish specific commands, meaning
/// it will work even if fish is not installed. This is used by read_i.
const DEFAULT_PROMPT: &wstr = L!("echo -n \"$USER@$hostname $PWD \"'> '");

/// The name of the function that prints the fish prompt.
const LEFT_PROMPT_FUNCTION_NAME: &wstr = L!("fish_prompt");

/// The name of the function that prints the fish right prompt (RPROMPT).
const RIGHT_PROMPT_FUNCTION_NAME: &wstr = L!("fish_right_prompt");

/// The name of the function to use in place of the left prompt if we're in the debugger context.
const DEBUG_PROMPT_FUNCTION_NAME: &wstr = L!("fish_breakpoint_prompt");

/// The name of the function for getting the input mode indicator.
const MODE_PROMPT_FUNCTION_NAME: &wstr = L!("fish_mode_prompt");

/// The default title for the reader. This is used by reader_readline.
const DEFAULT_TITLE: &wstr = L!("echo (status current-command) \" \" $PWD");

/// The maximum number of characters to read from the keyboard without repainting. Note that this
/// readahead will only occur if new characters are available for reading, fish will never block for
/// more input without repainting.
const READAHEAD_MAX: usize = 256;

/// Helper to get the generation count
pub fn read_generation_count() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// We try to ensure that syntax highlighting completes appropriately before executing what the user
/// typed. But we do not want it to block forever - e.g. it may hang on determining if an arbitrary
/// argument is a path. This is how long we'll wait (in milliseconds) before giving up and
/// performing a no-io syntax highlighting. See #7418, #5912.
const HIGHLIGHT_TIMEOUT_FOR_EXECUTION: Duration = Duration::from_millis(250);

/// The readers interrupt signal handler. Cancels all currently running blocks.
/// This is called from a signal handler!
pub fn reader_handle_sigint() {
    INTERRUPTED.store(SIGINT, Ordering::Relaxed);
}

/// Clear the interrupted flag unconditionally without handling anything. The flag could have been
/// set e.g. when an interrupt arrived just as we were ending an earlier \c reader_readline
/// invocation but before the \c is_interactive_read flag was cleared.
pub fn reader_reset_interrupted() {
    INTERRUPTED.store(0, Ordering::Relaxed);
}

/// Return the value of the interrupted flag, which is set by the sigint handler, and clear it if it
/// was set. In practice this will return 0 or SIGINT.
pub fn reader_test_and_clear_interrupted() -> i32 {
    let res = INTERRUPTED.load(Ordering::Relaxed);
    if res != 0 {
        INTERRUPTED.store(0, Ordering::Relaxed);
    };
    res
}

/// Mark that we encountered SIGHUP and must (soon) exit. This is invoked from a signal handler.
pub fn reader_sighup() {
    // Beware, we may be in a signal handler.
    SIGHUP_RECEIVED.store(true);
}

fn reader_received_sighup() -> bool {
    SIGHUP_RECEIVED.load()
}

impl ReaderData {
    fn new(history: Arc<History>, conf: ReaderConfig, is_top_level: bool) -> Pin<Box<Self>> {
        let input_data = InputData::new(conf.inputfd);
        let mut command_line = EditableLine::default();
        if is_top_level {
            let state = commandline_state_snapshot();
            command_line.push_edit(Edit::new(0..0, state.text.clone()), false);
            command_line.set_position(state.cursor_pos);
        }
        Pin::new(Box::new(Self {
            canary: Rc::new(()),
            conf,
            command_line,
            command_line_transient_edit: None,
            rendered_layout: Default::default(),
            autosuggestion: Default::default(),
            saved_autosuggestion: Default::default(),
            pager: Default::default(),
            current_page_rendering: Default::default(),
            suppress_autosuggestion: Default::default(),
            reset_loop_state: Default::default(),
            first_prompt: true,
            last_flash: Default::default(),
            flash_autosuggestion: false,
            screen: Screen::new(),
            input_data,
            queued_repaint: false,
            history,
            history_search: Default::default(),
            history_pager: None,
            cursor_selection_mode: CursorSelectionMode::Exclusive,
            cursor_end_mode: CursorEndMode::Exclusive,
            selection: Default::default(),
            left_prompt_buff: Default::default(),
            mode_prompt_buff: Default::default(),
            right_prompt_buff: Default::default(),
            cycle_command_line: Default::default(),
            cycle_cursor_pos: Default::default(),
            exit_loop_requested: Default::default(),
            did_warn_for_bg_jobs: Default::default(),
            kill_item: Default::default(),
            force_exec_prompt_and_repaint: Default::default(),
            last_jump_target: Default::default(),
            last_jump_direction: JumpDirection::Forward,
            last_jump_precision: JumpPrecision::To,
            in_flight_highlight_request: Default::default(),
            in_flight_autosuggest_request: Default::default(),
            rls: None,
        }))
    }

    // We repaint our prompt if fstat reports the tty as having changed.
    // But don't react to tty changes that we initiated, because of commands or
    // on-variable events (e.g. for fish_bind_mode). See #3481.
    pub fn save_screen_state(&mut self) {
        self.screen.save_status();
    }

    fn is_navigating_pager_contents(&self) -> bool {
        self.pager.is_navigating_contents() || self.history_pager.is_some()
    }

    fn edit_line(&self, elt: EditableLineTag) -> &EditableLine {
        match elt {
            EditableLineTag::Commandline => &self.command_line,
            EditableLineTag::SearchField => &self.pager.search_field_line,
        }
    }

    fn edit_line_mut(&mut self, elt: EditableLineTag) -> &mut EditableLine {
        match elt {
            EditableLineTag::Commandline => &mut self.command_line,
            EditableLineTag::SearchField => &mut self.pager.search_field_line,
        }
    }

    /// The line that is currently being edited. Typically the command line, but may be the search
    /// field.
    fn active_edit_line_tag(&self) -> EditableLineTag {
        if self.is_navigating_pager_contents() && self.pager.is_search_field_shown() {
            return EditableLineTag::SearchField;
        }
        EditableLineTag::Commandline
    }

    fn active_edit_line(&self) -> (EditableLineTag, &EditableLine) {
        let elt = self.active_edit_line_tag();
        (elt, self.edit_line(elt))
    }

    fn active_edit_line_mut(&mut self) -> (EditableLineTag, &mut EditableLine) {
        let elt = self.active_edit_line_tag();
        (elt, self.edit_line_mut(elt))
    }

    fn rls(&self) -> &ReadlineLoopState {
        self.rls.as_ref().unwrap()
    }
    fn rls_mut(&mut self) -> &mut ReadlineLoopState {
        self.rls.as_mut().unwrap()
    }

    /// Do what we need to do whenever our command line changes.
    fn command_line_changed(
        &mut self,
        elt: EditableLineTag,
        autosuggestion_update: AutosuggestionUpdate,
    ) {
        assert_is_main_thread();
        match elt {
            EditableLineTag::Commandline => {
                // Update the gen count.
                GENERATION.fetch_add(1, Ordering::Relaxed);
                let saved_autosuggestion = self.saved_autosuggestion.take();
                use AutosuggestionUpdate::*;
                match autosuggestion_update {
                    Preserve => (),
                    Remove => self.autosuggestion.clear(),
                    RemoveAndSave => {
                        self.saved_autosuggestion = Some(std::mem::take(&mut self.autosuggestion))
                    }
                    Restore => {
                        self.autosuggestion = saved_autosuggestion.unwrap();
                        self.suppress_autosuggestion = false;
                    }
                }
            }
            EditableLineTag::SearchField => {
                if self.history_pager.is_some() {
                    self.fill_history_pager(
                        HistoryPagerInvocation::Anew,
                        Some(SelectionMotion::Next),
                        SearchDirection::Backward,
                    );
                    return;
                }
                if self.pager.is_empty() {
                    return;
                }
                self.pager.refilter_completions();
                self.pager_selection_changed();
            }
        }
    }

    /// Reflect our current data in the command line state snapshot.
    fn update_commandline_state(&self) {
        let mut snapshot = commandline_state_snapshot();
        if snapshot.text != self.command_line.text() {
            snapshot.text = self.command_line.text().to_owned();
        }
        snapshot.cursor_pos = self.command_line.position();
        snapshot.history = Some(self.history.clone());
        snapshot.selection = self.get_selection();
        snapshot.pager_mode = !self.pager.is_empty();
        snapshot.pager_fully_disclosed = self.current_page_rendering.remaining_to_disclose == 0;
        if (snapshot.search_field.is_some() != self.pager.search_field_shown)
            || snapshot
                .search_field
                .as_ref()
                .is_some_and(|(text, position)| {
                    text != self.pager.search_field_line.text()
                        || *position != self.pager.search_field_line.position()
                })
        {
            snapshot.search_field = self.pager.search_field_shown.then(|| {
                (
                    self.pager.search_field_line.text().to_owned(),
                    self.pager.search_field_line.position(),
                )
            });
        }
        snapshot.search_mode = self.history_search.active();
    }

    /// Apply any changes from the reader snapshot. This is called after running fish script,
    /// incorporating changes from the commandline builtin.
    fn apply_commandline_state_changes(&mut self) {
        // Only the text and cursor position may be changed.
        let state = commandline_get_state(false);
        if state.text != self.command_line.text()
            || state.cursor_pos != self.command_line.position()
        {
            // The commandline builtin changed our contents.
            self.clear_pager();
            self.set_buffer_maintaining_pager(&state.text, state.cursor_pos);
            self.reset_loop_state = true;
        } else if let Some((new_search_field, new_cursor_pos)) = state.search_field {
            if !self.pager.search_field_shown {
                return; // Not yet supported.
            }
            if new_search_field == self.pager.search_field_line.text()
                && new_cursor_pos == self.pager.search_field_line.position()
            {
                return;
            }
            self.push_edit(
                EditableLineTag::SearchField,
                Edit::new(0..self.pager.search_field_line.len(), new_search_field),
            );
            self.pager.search_field_line.set_position(new_cursor_pos);
        }
    }

    /// Update the cursor position.
    fn update_buff_pos(&mut self, elt: EditableLineTag, mut new_pos: Option<usize>) -> bool {
        let el = self.edit_line(elt);
        if self.cursor_end_mode == CursorEndMode::Inclusive {
            let mut pos = new_pos.unwrap_or(el.position());
            if !el.is_empty() && pos == el.len() {
                pos = el.len() - 1;
                if el.position() == pos {
                    return false;
                }
                new_pos = Some(pos);
            }
        }
        let old_pos = el.position();
        if let Some(pos) = new_pos {
            self.edit_line_mut(elt).set_position(pos);
        }

        if elt != EditableLineTag::Commandline {
            return true;
        }
        // When moving across lines, hold off on autosuggestions until the next insertion.
        if let Some(new_pos) = new_pos {
            let range = if new_pos <= old_pos {
                new_pos..old_pos
            } else {
                old_pos..new_pos
            };
            if self.command_line.text()[range].contains('\n') {
                self.suppress_autosuggestion = true;
            }
        }
        let buff_pos = self.command_line.position();
        let target_char = if self.cursor_selection_mode == CursorSelectionMode::Inclusive {
            1
        } else {
            0
        };
        let Some(selection) = self.selection.as_mut() else {
            return true;
        };
        if selection.begin <= buff_pos {
            selection.start = selection.begin;
            selection.stop = buff_pos + target_char;
        } else {
            selection.start = buff_pos;
            selection.stop = selection.begin + target_char;
        }
        true
    }

    pub fn mouse_left_click(&mut self, cursor: ViewportPosition, click_position: ViewportPosition) {
        FLOG!(
            reader,
            "Cursor is at",
            cursor,
            "; received left mouse click at",
            click_position
        );
        match self
            .screen
            .offset_in_cmdline_given_cursor(click_position, cursor)
        {
            CharOffset::Cmd(new_pos) | CharOffset::Pointer(new_pos) => {
                let (elt, _el) = self.active_edit_line();
                self.update_buff_pos(elt, Some(new_pos));
            }
            CharOffset::Pager(idx) if self.pager.selected_completion_idx != Some(idx) => {
                self.pager.selected_completion_idx = Some(idx);
                self.pager_selection_changed();
            }
            _ => {}
        }
    }
}

/// Given a command line and an autosuggestion, return the string that gets shown to the user.
/// Exposed for testing purposes only.
pub fn combine_command_and_autosuggestion(
    cmdline: &wstr,
    line_range: Range<usize>,
    autosuggestion: &wstr,
) -> WString {
    // We want to compute the full line, containing the command line and the autosuggestion They may
    // disagree on whether characters are uppercase or lowercase.
    let pos = line_range.end;
    let full_line;
    assert!(!autosuggestion.is_empty());
    assert!(autosuggestion.len() >= line_range.len());
    let available = autosuggestion.len() - line_range.len();
    let line = &cmdline[line_range];

    if !string_prefixes_string(line, autosuggestion) {
        // We have an autosuggestion which is not a prefix of the command line, i.e. a case
        // disagreement. Decide whose case we want to use.
        assert!(string_prefixes_string_case_insensitive(
            line,
            autosuggestion
        ));
        // Here we do something funny: if the last token of the command line contains any uppercase
        // characters, we use its case. Otherwise we use the case of the autosuggestion. This
        // is an idea from issue #335.
        let (tok, _) = parse_util_token_extent(cmdline, cmdline.len() - 1);
        let last_token_contains_uppercase = cmdline[tok].chars().any(|c| c.is_uppercase());
        if !last_token_contains_uppercase {
            // Use the autosuggestion's case.
            let start: usize = unsafe {
                (line.as_char_slice().first().unwrap() as *const char)
                    .offset_from(&cmdline.as_char_slice()[0])
            }
            .try_into()
            .unwrap();
            full_line = cmdline[..start].to_owned() + autosuggestion + &cmdline[pos..];
            return full_line;
        }
    }
    // Use the command line case for its characters, then append the remaining characters in
    // the autosuggestion.
    cmdline[..pos].to_owned()
        + &autosuggestion[autosuggestion.len() - available..]
        + &cmdline[pos..]
}

impl<'a> Reader<'a> {
    pub(crate) fn blocking_query(&self) -> RefMut<'_, Option<TerminalQuery>> {
        self.parser.blocking_query.get().unwrap().borrow_mut()
    }

    pub fn request_cursor_position(&mut self, out: &mut Outputter, q: CursorPositionQuery) {
        if !isatty(STDOUT_FILENO) {
            return;
        }
        let mut query = self.blocking_query();
        assert!(query.is_none());
        *query = Some(TerminalQuery::CursorPositionReport(q));
        out.write_command(QueryCursorPosition);
        drop(query);
        self.save_screen_state();
    }

    /// Return true if the command line has changed and repainting is needed. If `colors` is not
    /// null, then also return true if the colors have changed.
    fn is_repaint_needed(&self, mcolors: Option<&[HighlightSpec]>) -> bool {
        // Note: this function is responsible for detecting all of the ways that the command line may
        // change, by comparing it to what is present in rendered_layout.
        // The pager is the problem child, it has its own update logic.
        let check = |val: bool, reason: &str| {
            if val {
                FLOG!(reader_render, "repaint needed because", reason, "change");
            }
            val
        };

        let focused_on_pager = self.active_edit_line_tag() == EditableLineTag::SearchField;
        let pager_search_field_position = focused_on_pager.then(|| self.pager.cursor_position());
        let last = &self.rendered_layout;
        check(self.force_exec_prompt_and_repaint, "forced")
            || check(self.command_line.text() != last.text, "text")
            || check(
                mcolors.is_some_and(|colors| colors != last.colors),
                "highlight",
            )
            || check(self.selection != last.selection, "selection")
            || check(self.command_line.position() != last.position, "position")
            || check(
                pager_search_field_position != last.pager_search_field_position,
                "pager_search_field_position",
            )
            || check(
                self.history_search.search_range_if_active() != last.history_search_range,
                "history search",
            )
            || check(
                self.autosuggestion.text != last.autosuggestion,
                "autosuggestion",
            )
            || check(
                self.left_prompt_buff != last.left_prompt_buff,
                "left_prompt",
            )
            || check(
                self.mode_prompt_buff != last.mode_prompt_buff,
                "mode_prompt",
            )
            || check(
                self.right_prompt_buff != last.right_prompt_buff,
                "right_prompt",
            )
            || check(
                self.pager
                    .rendering_needs_update(&self.current_page_rendering),
                "pager",
            )
    }

    /// Generate a new layout data from the current state of the world.
    /// If `mcolors` has a value, then apply it; otherwise extend existing colors.
    fn make_layout_data(&self) -> LayoutData {
        let mut result = LayoutData::default();
        let focused_on_pager = self.active_edit_line_tag() == EditableLineTag::SearchField;
        result.text = self.command_line.text().to_owned();
        result.colors = self.command_line.colors().to_vec();
        assert!(result.text.len() == result.colors.len());
        result.position = self.command_line.position();
        result.pager_search_field_position = focused_on_pager.then(|| self.pager.cursor_position());
        result.selection = self.selection;
        result.history_search_range = self.history_search.search_range_if_active();
        result.autosuggestion = self.autosuggestion.text.clone();
        result.left_prompt_buff = self.left_prompt_buff.clone();
        result.mode_prompt_buff = self.mode_prompt_buff.clone();
        result.right_prompt_buff = self.right_prompt_buff.clone();
        result
    }

    /// Generate a new layout data from the current state of the world, and paint with it.
    /// If `mcolors` has a value, then apply it; otherwise extend existing colors.
    fn layout_and_repaint(&mut self, reason: &wstr) {
        self.rendered_layout = self.make_layout_data();
        self.paint_layout(reason, false);
    }

    fn layout_and_repaint_before_execution(&mut self) {
        self.rendered_layout = self.make_layout_data();
        self.paint_layout(L!("prepare to execute"), true);
    }

    /// Paint the last rendered layout.
    /// `reason` is used in FLOG to explain why.
    fn paint_layout(&mut self, reason: &wstr, is_final_rendering: bool) {
        FLOGF!(reader_render, "Repainting from %ls", reason);
        let cmd_line = &self.data.command_line;

        let (full_line, autosuggested_range) = if self.conf.in_silent_mode {
            (
                Cow::Owned(
                    wstr::from_char_slice(&[get_obfuscation_read_char()]).repeat(cmd_line.len()),
                ),
                None,
            )
        } else if self.is_at_line_with_autosuggestion() {
            // Combine the command and autosuggestion into one string.
            let autosuggestion = &self.autosuggestion;
            let search_string_range = &autosuggestion.search_string_range;
            let autosuggested_start = search_string_range.end;
            let autosuggested_end = search_string_range.start + autosuggestion.text.len();
            (
                Cow::Owned(combine_command_and_autosuggestion(
                    cmd_line.text(),
                    autosuggestion.search_string_range.clone(),
                    &autosuggestion.text,
                )),
                Some(autosuggested_start..autosuggested_end),
            )
        } else {
            (Cow::Borrowed(cmd_line.text()), None)
        };
        let autosuggested_range = autosuggested_range.unwrap_or(full_line.len()..full_line.len());

        // Copy the colors and insert the autosuggestion color.
        let data = &self.data.rendered_layout;
        let mut colors = data.colors.clone();

        // Highlight any history search.
        if !self.conf.in_silent_mode && data.history_search_range.is_some() {
            let mut range = data.history_search_range.unwrap().as_usize();
            if range.end > colors.len() {
                range.start = range.start.min(colors.len());
                range.end = colors.len();
            }

            let explicit_foreground = self
                .vars()
                .get_unless_empty(L!("fish_color_search_match"))
                .is_some_and(|var| parse_text_face(var.as_list()).fg.is_some());

            for color in &mut colors[range] {
                if explicit_foreground {
                    color.foreground = HighlightRole::search_match;
                }
                color.background = HighlightRole::search_match;
            }
        }

        // Apply any selection.
        if let Some(selection) = data.selection {
            let selection_color = HighlightSpec::with_both(HighlightRole::selection);
            let end = std::cmp::min(selection.stop, colors.len());
            for color in &mut colors[selection.start.min(end)..end] {
                *color = selection_color;
            }
        }

        // Extend our colors with the autosuggestion.
        let pos = autosuggested_range.start;
        colors.splice(
            pos..pos,
            vec![
                if self.flash_autosuggestion {
                    HighlightSpec::with_both(HighlightRole::search_match)
                } else {
                    HighlightSpec::with_fg(HighlightRole::autosuggestion)
                };
                autosuggested_range.len()
            ],
        );

        // Compute the indentation, then extend it with 0s for the autosuggestion. The autosuggestion
        // always conceptually has an indent of 0.
        let mut indents = parse_util_compute_indents(cmd_line.text());
        indents.splice(pos..pos, vec![0; autosuggested_range.len()]);

        let screen = &mut self.data.screen;
        let pager = &mut self.data.pager;
        let current_page_rendering = &mut self.data.current_page_rendering;
        screen.write(
            // Prepend the mode prompt to the left prompt.
            &(self.data.mode_prompt_buff.clone() + &self.data.left_prompt_buff[..]),
            &self.data.right_prompt_buff,
            &full_line,
            autosuggested_range,
            colors,
            indents,
            data.position,
            data.pager_search_field_position,
            self.parser.vars(),
            pager,
            current_page_rendering,
            is_final_rendering,
        );
    }
}

enum AutosuggestionUpdate {
    Preserve,
    Remove,
    RemoveAndSave,
    Restore,
}

impl ReaderData {
    /// Internal helper function for handling killing parts of text.
    fn kill(&mut self, elt: EditableLineTag, range: Range<usize>, mode: Kill, newv: bool) {
        let text = match elt {
            EditableLineTag::Commandline => &self.command_line,
            EditableLineTag::SearchField => &self.pager.search_field_line,
        }
        .text();
        let kill_item = &mut self.kill_item;
        if newv {
            *kill_item = text[range.clone()].to_owned();
            kill_add(kill_item.clone());
        } else {
            let old = kill_item.to_owned();
            match mode {
                Kill::Append => kill_item.push_utfstr(&text[range.clone()]),
                Kill::Prepend => {
                    *kill_item = text[range.clone()].to_owned();
                    kill_item.push_utfstr(&old);
                }
            }

            kill_replace(&old, kill_item.clone());
        }
        self.erase_substring(elt, range);
    }

    /// Insert the characters of the string into the command line buffer and print them to the screen
    /// using syntax highlighting, etc.
    /// Returns true if the string changed.
    fn insert_string(&mut self, elt: EditableLineTag, s: &wstr) {
        let history_search_active = self.history_search.active();
        let el = self.edit_line(elt);
        self.push_edit_internal(
            elt,
            Edit::new(el.position()..el.position(), s.to_owned()),
            /*allow_coalesce=*/ !history_search_active,
        );
        if elt == EditableLineTag::Commandline {
            self.command_line_transient_edit = None;
            self.suppress_autosuggestion = false;
        }
    }

    /// Erase @length characters starting at @offset.
    fn erase_substring(&mut self, elt: EditableLineTag, range: Range<usize>) {
        self.push_edit(elt, Edit::new(range, L!("").to_owned()));
    }

    fn clear(&mut self, elt: EditableLineTag) {
        let el = self.edit_line(elt);
        if !el.is_empty() {
            self.erase_substring(elt, 0..el.len());
        }
    }

    /// Replace the text of length @length at @offset by @replacement.
    fn replace_substring(
        &mut self,
        elt: EditableLineTag,
        range: Range<usize>,
        replacement: WString,
    ) {
        self.push_edit(elt, Edit::new(range, replacement));
    }

    fn push_edit(&mut self, elt: EditableLineTag, edit: Edit) {
        self.push_edit_internal(elt, edit, /*allow_coalesce=*/ false);
    }

    /// Insert the character into the command line buffer and print it to the screen using syntax
    /// highlighting, etc.
    fn insert_char(&mut self, elt: EditableLineTag, c: char) {
        self.insert_string(elt, &WString::from_chars([c]));
    }

    /// Set the specified string as the current buffer.
    fn set_command_line_and_position(
        &mut self,
        elt: EditableLineTag,
        new_str: WString,
        pos: usize,
    ) {
        self.push_edit(elt, Edit::new(0..self.edit_line(elt).len(), new_str));
        self.edit_line_mut(elt).set_position(pos);
        self.update_buff_pos(elt, Some(pos));
    }

    fn try_apply_edit_to_autosuggestion(&mut self, edit: &Edit) -> bool {
        let autosuggestion = &self.autosuggestion;
        if autosuggestion.is_empty() {
            return false;
        }

        // Check to see if our autosuggestion still applies; if so, don't recompute it.
        // Since the autosuggestion computation is asynchronous, this avoids "flashing" as you type into
        // the autosuggestion.
        // This is also the main mechanism by which readline commands that don't change the command line
        // text avoid recomputing the autosuggestion.
        assert!(string_prefixes_string_maybe_case_insensitive(
            autosuggestion.icase,
            &self.command_line.text()[autosuggestion.search_string_range.clone()],
            &autosuggestion.text
        ));
        let search_string_range = autosuggestion.search_string_range.clone();

        // This is a heuristic with false negatives but that seems fine.
        let Some(offset) = edit.range.start.checked_sub(search_string_range.start) else {
            return false;
        };
        let Some(remaining) = autosuggestion.text.get(offset..) else {
            return false;
        };
        if edit.range.end != search_string_range.end
            || !string_prefixes_string_maybe_case_insensitive(
                autosuggestion.icase,
                &edit.replacement,
                remaining,
            )
            || edit.replacement.len() == remaining.len()
        {
            return false;
        }
        self.autosuggestion.search_string_range.end = search_string_range.end
            - edit.range.len().min(search_string_range.end)
            + edit.replacement.len();
        true
    }

    fn push_edit_internal(&mut self, elt: EditableLineTag, edit: Edit, allow_coalesce: bool) {
        let mut autosuggestion_update = AutosuggestionUpdate::Remove;
        if elt == EditableLineTag::Commandline {
            let preserves_autosuggestion = self.try_apply_edit_to_autosuggestion(&edit);
            if preserves_autosuggestion {
                autosuggestion_update = AutosuggestionUpdate::Preserve
            } else if !self.autosuggestion.is_empty()
                && edit.range.start == self.autosuggestion.search_string_range.end
                && edit.range.is_empty()
                && !edit.replacement.is_empty()
            {
                // When inserting at the autosuggestion something that doesn't match, save it.
                autosuggestion_update = AutosuggestionUpdate::RemoveAndSave;
            } else if self
                .saved_autosuggestion
                .as_ref()
                .is_some_and(|saved_autosuggestion| {
                    self.conf.autosuggest_ok
                        && self.history_search.is_at_present()
                        && edit.replacement.is_empty()
                        && edit.range.start == saved_autosuggestion.search_string_range.end
                        && !edit.range.is_empty()
                })
            {
                autosuggestion_update = AutosuggestionUpdate::Restore;
            }
        }
        self.edit_line_mut(elt).push_edit(edit, allow_coalesce);
        self.command_line_changed(elt, autosuggestion_update);
    }

    fn undo(&mut self, elt: EditableLineTag) -> bool {
        let ok = self.edit_line_mut(elt).undo();
        if ok {
            self.command_line_changed(elt, AutosuggestionUpdate::Remove);
        }
        ok
    }
    fn redo(&mut self, elt: EditableLineTag) -> bool {
        let ok = self.edit_line_mut(elt).redo();
        if ok {
            self.command_line_changed(elt, AutosuggestionUpdate::Remove);
        }
        ok
    }

    /// Undo the transient edit und update commandline accordingly.
    fn clear_transient_edit(&mut self) {
        if self.command_line_transient_edit.is_none() {
            return;
        }
        self.undo(EditableLineTag::Commandline);
        self.update_buff_pos(EditableLineTag::Commandline, None);
        self.command_line_transient_edit = None;
    }

    fn replace_current_token(&mut self, new_token: WString) {
        // Find current token.
        let (elt, el) = self.active_edit_line();
        let (token_range, _) = parse_util_token_extent(el.text(), el.position());

        self.replace_substring(elt, token_range, new_token);
    }

    /// Apply the history search to the command line.
    fn update_command_line_from_history_search(&mut self) {
        assert!(self.history_search.active());
        if let Some(transient_edit) = self.command_line_transient_edit.take() {
            if transient_edit == TransientEdit::HistorySearch {
                self.undo(EditableLineTag::Commandline);
            }
        }
        if !self.history_search.is_at_present() {
            let new_text = self.history_search.current_result().to_owned();
            if self.history_search.by_token() {
                self.replace_current_token(new_text);
            } else {
                self.replace_substring(
                    EditableLineTag::Commandline,
                    0..self.command_line.len(),
                    new_text,
                );
                if self.history_search.by_prefix()
                    && !self.history_search.search_string().is_empty()
                {
                    self.command_line
                        .set_position(self.history_search.search_string().len());
                }
            }
            self.command_line_transient_edit = Some(TransientEdit::HistorySearch);
        }
        self.update_buff_pos(EditableLineTag::Commandline, None);
    }

    /// Remove the previous character in the character buffer and on the screen using syntax
    /// highlighting, etc.
    fn delete_char(&mut self, backward: bool /* = true */) {
        let (elt, el) = self.active_edit_line();

        let mut pos = el.position();
        if !backward {
            pos += 1;
        }
        let pos_end = pos;

        if el.position() == 0 && backward {
            return;
        }

        // Fake composed character sequences by continuing to delete until we delete a character of
        // width at least 1.
        let mut width;
        loop {
            pos -= 1;
            width = fish_wcwidth(el.text().char_at(pos));
            if width != 0 || pos == 0 {
                break;
            }
        }
        self.suppress_autosuggestion = true;
        self.erase_substring(elt, pos..pos_end);
        self.update_buff_pos(elt, None);
    }
}

#[derive(Eq, PartialEq)]
enum MoveWordDir {
    Left,
    Right,
}

impl ReaderData {
    /// Move buffer position one word or erase one word. This function updates both the internal buffer
    /// and the screen. It is used by M-left, M-right and ^W to do block movement or block erase.
    ///
    /// \param move_right true if moving right
    /// \param erase Whether to erase the characters along the way or only move past them.
    /// \param newv if the new kill item should be appended to the previous kill item or not.
    fn move_word(
        &mut self,
        elt: EditableLineTag,
        direction: MoveWordDir,
        erase: bool,
        style: MoveWordStyle,
        newv: bool,
    ) {
        let move_right = direction == MoveWordDir::Right;
        // Return if we are already at the edge.
        let el = self.edit_line(elt);
        let boundary = if move_right { el.len() } else { 0 };
        if el.position() == boundary {
            return;
        }

        // When moving left, a value of 1 means the character at index 0.
        let mut state = MoveWordStateMachine::new(style);
        let start_buff_pos = el.position();

        let mut buff_pos = el.position();
        while buff_pos != boundary {
            let idx = if move_right { buff_pos } else { buff_pos - 1 };
            let c = el.at(idx);
            if !state.consume_char(c) {
                break;
            }
            buff_pos = if move_right {
                buff_pos + 1
            } else {
                buff_pos - 1
            };
        }

        // Always consume at least one character.
        if buff_pos == start_buff_pos {
            buff_pos = if move_right {
                buff_pos + 1
            } else {
                buff_pos - 1
            };
        }

        // If we are moving left, buff_pos-1 is the index of the first character we do not delete
        // (possibly -1). If we are moving right, then buff_pos is that index - possibly el->size().
        if erase {
            // Don't autosuggest after a kill.
            if elt == EditableLineTag::Commandline {
                self.suppress_autosuggestion = true;
            }

            if move_right {
                self.kill(elt, start_buff_pos..buff_pos, Kill::Append, newv);
            } else {
                self.kill(elt, buff_pos..start_buff_pos, Kill::Prepend, newv);
            }
        } else {
            self.update_buff_pos(elt, Some(buff_pos));
        }
    }

    fn jump_to_matching_bracket(
        &mut self,
        precision: JumpPrecision,
        elt: EditableLineTag,
        jump_from: usize,
        l_bracket: char,
        r_bracket: char,
    ) -> bool {
        let el = self.edit_line(elt);
        let mut tmp_r_pos: usize = 0;
        let mut brackets_stack = Vec::new();
        while tmp_r_pos < el.len() {
            if el.at(tmp_r_pos) == l_bracket {
                brackets_stack.push(tmp_r_pos);
            } else if el.at(tmp_r_pos) == r_bracket {
                match brackets_stack.pop() {
                    Some(tmp_l_pos) if jump_from == tmp_l_pos => {
                        return match precision {
                            JumpPrecision::Till => self.update_buff_pos(elt, Some(tmp_r_pos - 1)),
                            JumpPrecision::To => self.update_buff_pos(elt, Some(tmp_r_pos)),
                        };
                    }
                    Some(tmp_l_pos) if jump_from == tmp_r_pos => {
                        return match precision {
                            JumpPrecision::Till => self.update_buff_pos(elt, Some(tmp_l_pos + 1)),
                            JumpPrecision::To => self.update_buff_pos(elt, Some(tmp_l_pos)),
                        };
                    }
                    _ => {}
                }
            }
            tmp_r_pos += 1;
        }
        return false;
    }

    fn jump_and_remember_last_jump(
        &mut self,
        direction: JumpDirection,
        precision: JumpPrecision,
        elt: EditableLineTag,
        target: char,
        skip_till: bool,
    ) -> bool {
        self.last_jump_target = Some(target);
        self.last_jump_direction = direction;
        self.last_jump_precision = precision;
        self.jump(direction, precision, elt, vec![target], skip_till)
    }

    fn jump(
        &mut self,
        direction: JumpDirection,
        precision: JumpPrecision,
        elt: EditableLineTag,
        targets: Vec<char>,
        skip_till: bool,
    ) -> bool {
        let el = self.edit_line(elt);

        match direction {
            JumpDirection::Backward => {
                let mut tmp_pos = el.position();
                if precision == JumpPrecision::Till && skip_till && tmp_pos > 0 {
                    tmp_pos -= 1;
                }

                loop {
                    if tmp_pos == 0 {
                        return false;
                    }
                    tmp_pos -= 1;
                    if targets.iter().any(|&target| el.at(tmp_pos) == target) {
                        if precision == JumpPrecision::Till {
                            tmp_pos = std::cmp::min(el.len() - 1, tmp_pos + 1);
                        }
                        self.update_buff_pos(elt, Some(tmp_pos));
                        return true;
                    }
                }
            }
            JumpDirection::Forward => {
                let mut tmp_pos = el.position() + 1;
                if precision == JumpPrecision::Till && skip_till && tmp_pos < el.len() - 1 {
                    tmp_pos += 1;
                }

                while tmp_pos < el.len() {
                    if targets.iter().any(|&target| el.at(tmp_pos) == target) {
                        if precision == JumpPrecision::Till {
                            tmp_pos -= 1;
                        }
                        self.update_buff_pos(elt, Some(tmp_pos));
                        return true;
                    }
                    tmp_pos += 1;
                }
                return false;
            }
        }
    }
}

impl<'a> Reader<'a> {
    /// Read a command to execute, respecting input bindings.
    /// Return the command, or none if we were asked to cancel (e.g. SIGHUP).
    fn readline(&mut self, nchars: Option<NonZeroUsize>) -> Option<WString> {
        let mut tty = TtyHandoff::new();

        self.rls = Some(ReadlineLoopState::new());

        // Suppress fish_trace during executing key bindings.
        // This is simply to reduce noise.
        let _restore = self.parser.push_scope(|s| s.suppress_fish_trace = true);

        // If nchars_or_0 is positive, then that's the maximum number of chars. Otherwise keep it at
        // SIZE_MAX.
        self.rls_mut().nchars = nchars;

        // The command line before completion.
        self.cycle_command_line.clear();
        self.cycle_cursor_pos = 0;

        self.history_search.reset();

        // It may happen that a command we ran when job control was disabled nevertheless stole the tty
        // from us. In that case when we read from our fd, it will trigger SIGTTIN. So just
        // unconditionally reclaim the tty. See #9181.
        unsafe { libc::tcsetpgrp(self.conf.inputfd, libc::getpgrp()) };

        // Get the current terminal modes. These will be restored when the function returns.
        let mut old_modes = MaybeUninit::uninit();
        let restore_modes =
            unsafe { libc::tcgetattr(self.conf.inputfd, old_modes.as_mut_ptr()) } == 0;

        // Set the new modes.
        if unsafe { libc::tcsetattr(self.conf.inputfd, TCSANOW, &*shell_modes()) } == -1 {
            let err = errno().0;
            // This check is required to work around certain issues with fish's approach to
            // terminal control when launching interactive processes while in non-interactive
            // mode. See #4178 for one such example.
            if err != ENOTTY || is_interactive_session() {
                perror("tcsetattr");
            }
        }

        initial_query(
            &self.parser.blocking_query,
            &mut BufferedOutputter::new(Outputter::stdoutput()),
            Some(self.parser.vars()),
        );

        // HACK: Don't abandon line for the first prompt, because
        // if we're started with the terminal it might not have settled,
        // so the width is quite likely to be in flight.
        //
        // This means that `printf %s foo; fish` will overwrite the `foo`,
        // but that's a smaller problem than having the omitted newline char
        // appear constantly.
        //
        // I can't see a good way around this.
        if !self.first_prompt {
            self.screen
                .reset_abandoning_line(usize::try_from(termsize_last().width).unwrap());
        }
        self.first_prompt = false;

        if !self.conf.event.is_empty() {
            event::fire_generic(self.parser, self.conf.event.to_owned(), vec![]);
        }
        self.exec_prompt(true, false);

        // Start out as initially dirty.
        self.force_exec_prompt_and_repaint = true;

        while !self.rls().finished && !check_exit_loop_maybe_warning(Some(self)) {
            // Enable tty protocols while we read input.
            tty.enable_tty_protocols();
            if self.handle_char_event(None).is_break() {
                break;
            }
        }

        // Disable tty protocols now that we're going to execute a command.
        if tty.disable_tty_protocols() {
            self.save_screen_state();
        }

        if self.conf.transient_prompt {
            self.exec_prompt(true, true);
        }

        // Redraw the command line. This is what ensures the autosuggestion is hidden, etc. after the
        // user presses enter.
        if self.is_repaint_needed(None)
            || self.screen.scrolled()
            || self.conf.inputfd != STDIN_FILENO
        {
            self.layout_and_repaint_before_execution();
        }

        // Finish syntax highlighting (but do not wait forever).
        if self.rls().finished {
            self.finish_highlighting_before_exec();
        }

        // Emit a newline so that the output is on the line after the command.
        // But do not emit a newline if the cursor has wrapped onto a new line all its own - see #6826.
        if !self.screen.cursor_is_wrapped_to_own_line() {
            let _ = write_to_fd(b"\n", STDOUT_FILENO);
        }

        // HACK: If stdin isn't the same terminal as stdout, we just moved the cursor.
        // For now, just reset it to the beginning of the line.
        if self.conf.inputfd != STDIN_FILENO {
            let _ = write_loop(&STDOUT_FILENO, b"\r");
        }

        // Ensure we have no pager contents when we exit.
        if !self.pager.is_empty() {
            // Clear to end of screen to erase the pager contents.
            screen_force_clear_to_end();
            self.clear_pager();
        }

        if EXIT_STATE.load(Ordering::Relaxed) != ExitState::FinishedHandlers as _ {
            // The order of the two conditions below is important. Try to restore the mode
            // in all cases, but only complain if interactive.
            if restore_modes
                && unsafe { libc::tcsetattr(self.conf.inputfd, TCSANOW, old_modes.as_ptr()) } == -1
                && is_interactive_session()
            {
                perror("tcsetattr");
            }
            Outputter::stdoutput().borrow_mut().reset_text_face();
        }
        let result = self
            .rls()
            .finished
            .then(|| self.command_line.text().to_owned());
        self.rls = None;
        result
    }

    fn eval_bind_cmd(&mut self, cmd: &wstr) {
        let last_statuses = self.parser.vars().get_last_statuses();
        let prev_exec_external_count = self.parser.libdata().exec_external_count;
        // Disable TTY protocols while we run a bind command, because it may call out.
        let mut scoped_tty = TtyHandoff::new();
        let mut modified_tty = scoped_tty.disable_tty_protocols();

        self.parser.eval(cmd, &IoChain::new());
        self.parser.set_last_statuses(last_statuses);
        modified_tty |= scoped_tty.reclaim();
        if modified_tty
            || (self.parser.libdata().exec_external_count != prev_exec_external_count
                && self.data.left_prompt_buff.contains('\n'))
        {
            self.save_screen_state();
        }
    }

    /// Run a sequence of commands from an input binding.
    fn run_input_command_scripts(&mut self, cmd: &wstr) {
        self.eval_bind_cmd(cmd);

        // Restore tty to shell modes.
        // Some input commands will take over the tty - see #2114 for an example where vim is invoked
        // from a key binding. However we do NOT want to invoke term_donate(), because that will enable
        // ECHO mode, causing a race between new input and restoring the mode (#7770). So we leave the
        // tty alone, run the commands in shell mode, and then restore shell modes.
        let mut res;
        loop {
            res = unsafe { libc::tcsetattr(STDIN_FILENO, TCSANOW, &*shell_modes()) };
            if res >= 0 || errno().0 != EINTR {
                break;
            }
        }
        if res < 0 {
            perror("tcsetattr");
        }
        termsize_invalidate_tty();
    }

    /// Read normal characters, inserting them into the command line.
    /// Return the next unhandled event.
    fn read_normal_chars(&mut self) -> Option<CharEvent> {
        let mut event_needing_handling = None;
        let limit = std::cmp::min(
            self.rls().nchars.map_or(usize::MAX, |nchars| {
                usize::from(nchars) - self.command_line_len()
            }),
            READAHEAD_MAX,
        );

        let mut accumulated_chars = WString::new();

        while accumulated_chars.len() < limit {
            let evt = self.read_char();
            let CharEvent::Key(kevt) = &evt else {
                event_needing_handling = Some(evt);
                break;
            };
            if !poll_fd_readable(self.conf.inputfd) {
                event_needing_handling = Some(evt);
                break;
            }
            if kevt.input_style == CharInputStyle::NotFirst
                && accumulated_chars.is_empty()
                && self.active_edit_line().1.position() == 0
            {
                // The cursor is at the beginning and nothing is accumulated, so skip this character.
                continue;
            }

            if let Some(c) = kevt.key.codepoint_text() {
                accumulated_chars.push(c);
            } else {
                continue;
            };
        }

        if !accumulated_chars.is_empty() {
            let (elt, _el) = self.active_edit_line();
            self.insert_string(elt, &accumulated_chars);

            // End paging upon inserting into the normal command line.
            if elt == EditableLineTag::Commandline {
                self.clear_pager();
            }

            // Since we handled a normal character, we don't have a last command.
            self.rls_mut().last_cmd = None;
        }

        event_needing_handling
    }

    // A helper that kicks off syntax highlighting, autosuggestion computing, and repaints.
    fn color_suggest_repaint_now(&mut self) {
        if self.conf.inputfd == STDIN_FILENO {
            self.update_autosuggestion();
            self.super_highlight_me_plenty();
        }
        if self.is_repaint_needed(None) {
            self.layout_and_repaint(L!("toplevel"));
        }
        self.force_exec_prompt_and_repaint = false;
    }

    fn handle_char_event(&mut self, injected_event: Option<CharEvent>) -> ControlFlow<()> {
        if self.reset_loop_state {
            self.reset_loop_state = false;
            self.rls_mut().last_cmd = None;
            self.rls_mut().complete_did_insert = false;
        }
        // Perhaps update the termsize. This is cheap if it has not changed.
        self.update_termsize();

        // Repaint as needed.
        self.color_suggest_repaint_now();

        if self
            .rls()
            .nchars
            .is_some_and(|nchars| usize::from(nchars) <= self.command_line_len())
        {
            // We've already hit the specified character limit.
            self.rls_mut().finished = true;
            return ControlFlow::Break(());
        }

        let event_needing_handling = injected_event.or_else(|| loop {
            let event_needing_handling = self.read_normal_chars();
            if event_needing_handling.is_some() {
                break event_needing_handling;
            }
            if self
                .rls()
                .nchars
                .is_some_and(|nchars| usize::from(nchars) <= self.command_line_len())
            {
                break None;
            }
        });

        // If we ran `exit` anywhere, exit.
        self.exit_loop_requested |= self.parser.libdata().exit_current_script;
        self.parser.libdata_mut().exit_current_script = false;
        if self.exit_loop_requested {
            return ControlFlow::Continue(());
        }

        let Some(event_needing_handling) = event_needing_handling else {
            return ControlFlow::Continue(());
        };

        match event_needing_handling {
            CharEvent::Readline(readline_cmd_evt) => {
                if !matches!(
                    self.rls().last_cmd,
                    Some(ReadlineCmd::Yank | ReadlineCmd::YankPop)
                ) {
                    self.rls_mut().yank_len = 0;
                }

                let readline_cmd = readline_cmd_evt.cmd;
                if readline_cmd == ReadlineCmd::Cancel && self.is_navigating_pager_contents() {
                    self.clear_transient_edit();
                }

                // Clear the pager if necessary.
                let focused_on_search_field =
                    self.active_edit_line_tag() == EditableLineTag::SearchField;
                if !self.history_search.active()
                    && command_ends_paging(readline_cmd, focused_on_search_field)
                {
                    self.clear_pager();
                }

                self.handle_readline_command(readline_cmd);

                if self.history_search.active() && command_ends_history_search(readline_cmd) {
                    // "cancel" means to abort the whole thing, other ending commands mean to finish the
                    // search.
                    if readline_cmd == ReadlineCmd::Cancel {
                        // Go back to the search string by simply undoing the history-search edit.
                        self.clear_transient_edit();
                    }
                    self.history_search.reset();
                    self.command_line_transient_edit = None;
                }

                self.rls_mut().last_cmd = Some(readline_cmd);
            }
            CharEvent::Command(command) => {
                self.run_input_command_scripts(&command);
            }
            CharEvent::Key(kevt) => {
                // Ordinary char.
                if kevt.input_style == CharInputStyle::NotFirst
                    && self.active_edit_line().1.position() == 0
                {
                    // This character is skipped.
                } else {
                    // Regular character.
                    let (elt, _el) = self.active_edit_line();
                    if let Some(c) = kevt.key.codepoint_text() {
                        self.insert_char(elt, c);

                        if elt == EditableLineTag::Commandline {
                            self.clear_pager();
                            // We end history search. We could instead update the search string.
                            self.history_search.reset();
                        }
                    }
                }
                self.rls_mut().last_cmd = None;
            }
            CharEvent::Implicit(implicit_event) => match implicit_event {
                ImplicitEvent::Eof => {
                    reader_sighup();
                }
                ImplicitEvent::CheckExit => (),
                ImplicitEvent::FocusIn => {
                    event::fire_generic(self.parser, L!("fish_focus_in").to_owned(), vec![]);
                }
                ImplicitEvent::FocusOut => {
                    event::fire_generic(self.parser, L!("fish_focus_out").to_owned(), vec![]);
                }
                ImplicitEvent::DisableMouseTracking => {
                    Outputter::stdoutput()
                        .borrow_mut()
                        .write_command(DecrstMouseTracking);
                    self.save_screen_state();
                }
                ImplicitEvent::MouseLeft(position) => {
                    FLOG!(reader, "Mouse left click", position);
                    self.request_cursor_position(
                        &mut Outputter::stdoutput().borrow_mut(),
                        CursorPositionQuery::MouseLeft(position),
                    );
                }
            },
            CharEvent::QueryResponse(query_response) => {
                match query_response {
                    QueryResponseEvent::PrimaryDeviceAttribute => {
                        if *self.blocking_query() != Some(TerminalQuery::PrimaryDeviceAttribute) {
                            // Rogue reply.
                            return ControlFlow::Continue(());
                        }
                        if get_kitty_keyboard_capability() == Capability::Unknown {
                            set_kitty_keyboard_capability(Capability::NotSupported);
                            // We may have written to the tty, so save the screen state
                            // so we don't repaint.
                            self.screen.save_status();
                        }
                    }
                    QueryResponseEvent::CursorPositionReport(cursor_pos) => {
                        let cursor_pos_query = match &*self.blocking_query() {
                            Some(TerminalQuery::CursorPositionReport(cursor_pos_query)) => {
                                cursor_pos_query.clone()
                            }
                            _ => return ControlFlow::Continue(()), // Rogue reply.
                        };
                        match cursor_pos_query {
                            CursorPositionQuery::MouseLeft(click_position) => {
                                self.mouse_left_click(cursor_pos, click_position);
                            }
                            CursorPositionQuery::ScrollbackPush => {
                                self.screen.push_to_scrollback(cursor_pos.y);
                            }
                        }
                    }
                }
                let ok = stop_query(self.blocking_query());
                assert!(ok);
            }
        }
        ControlFlow::Continue(())
    }
}

fn send_xtgettcap_query(out: &mut impl Output, cap: &'static str) {
    if should_flog!(reader) {
        let mut tmp = Vec::<u8>::new();
        tmp.write_command(QueryXtgettcap(cap));
        FLOG!(
            reader,
            format!("Sending XTGETTCAP request for {}: {:?}", cap, tmp)
        );
    }
    out.write_command(QueryXtgettcap(cap));
}

#[allow(renamed_and_removed_lints)]
#[allow(clippy::blocks_in_if_conditions)] // for old clippy
fn query_capabilities_via_dcs(out: &mut impl Output, vars: &dyn Environment) {
    if vars.get_unless_empty(L!("STY")).is_some()
        || vars.get_unless_empty(L!("TERM")).is_some_and(|term| {
            let term = &term.as_list()[0];
            term == "screen" || term == "screen-256color"
        })
    {
        return;
    }
    out.write_command(DecsetAlternateScreenBuffer); // enable alternative screen buffer
    send_xtgettcap_query(out, SCROLL_FORWARD_TERMINFO_CODE);
    out.write_command(DecrstAlternateScreenBuffer); // disable alternative screen buffer
}

impl<'a> Reader<'a> {
    // Convenience cover to return the length of the command line.
    fn command_line_len(&self) -> usize {
        self.data.command_line.len()
    }

    // Convenience cover over ReaderData::update_buff_pos.
    fn update_buff_pos(&mut self, elt: EditableLineTag, new_pos: Option<usize>) -> bool {
        self.data.update_buff_pos(elt, new_pos)
    }

    // Convenience cover over ReaderData::push_edit.
    fn push_edit(&mut self, elt: EditableLineTag, edit: Edit) {
        self.data.push_edit(elt, edit);
    }

    fn handle_readline_command(&mut self, c: ReadlineCmd) {
        #[allow(non_camel_case_types)]
        type rl = ReadlineCmd;
        match c {
            rl::BeginningOfLine => {
                // Go to beginning of line.
                loop {
                    let (elt, el) = self.active_edit_line();
                    let position = {
                        let position = el.position();
                        if position == 0 || el.text().char_at(position - 1) == '\n' {
                            break;
                        }
                        position
                    };
                    self.update_buff_pos(elt, Some(position - 1));
                }
            }
            rl::EndOfLine => {
                if self.is_at_autosuggestion() {
                    self.accept_autosuggestion(AutosuggestionPortion::Count(usize::MAX));
                } else if !self.is_at_end() {
                    loop {
                        let position = {
                            let (_elt, el) = self.active_edit_line();
                            let position = el.position();
                            if position == el.len() {
                                break;
                            }
                            if el.text().char_at(position) == '\n' {
                                break;
                            }
                            position
                        };
                        if !self
                            .data
                            .update_buff_pos(self.active_edit_line_tag(), Some(position + 1))
                        {
                            break;
                        }
                    }
                }
            }
            rl::BeginningOfBuffer => {
                self.data
                    .update_buff_pos(EditableLineTag::Commandline, Some(0));
            }
            rl::EndOfBuffer => {
                self.data
                    .update_buff_pos(EditableLineTag::Commandline, Some(self.command_line_len()));
            }
            rl::CancelCommandline | rl::ClearCommandline => {
                if self.conf.exit_on_interrupt {
                    self.parser
                        .set_last_statuses(Statuses::just(STATUS_CMD_ERROR));
                    self.exit_loop_requested = true;
                    return;
                }
                if self.command_line.is_empty() {
                    return;
                }
                if c == rl::CancelCommandline {
                    // Move cursor to the end of the line.
                    let end = self.command_line.len();
                    {
                        let tmp =
                            std::mem::replace(&mut self.cursor_end_mode, CursorEndMode::Exclusive);
                        self.update_buff_pos(EditableLineTag::Commandline, Some(end));
                        self.cursor_end_mode = tmp;
                    }

                    self.autosuggestion.clear();
                    // Repaint also changes the actual cursor position
                    if self.is_repaint_needed(None) {
                        self.layout_and_repaint(L!("cancel"));
                    }

                    let mut outp = Outputter::stdoutput().borrow_mut();
                    if let Some(fish_color_cancel) = self.vars().get(L!("fish_color_cancel")) {
                        outp.set_text_face(parse_text_face_for_highlight(&fish_color_cancel));
                    }
                    outp.write_wstr(L!("^C"));
                    outp.reset_text_face();

                    // We print a newline last so the prompt_sp hack doesn't get us.
                    outp.push(b'\n');
                }
                self.push_edit(
                    EditableLineTag::Commandline,
                    Edit::new(0..self.command_line_len(), L!("").to_owned()),
                );
                if c == rl::CancelCommandline {
                    self.screen
                        .reset_abandoning_line(usize::try_from(termsize_last().width).unwrap());
                }

                // Post fish_cancel.
                event::fire_generic(self.parser, L!("fish_cancel").to_owned(), vec![]);
            }
            rl::Cancel => {
                // If we last inserted a completion, undo it.
                // This doesn't apply if the completion was selected via the pager
                // (in which case the last command is "execute" or similar,
                // but never complete{,_and_search})
                //
                // Also paging is already cancelled above.
                if self.rls().complete_did_insert
                    && matches!(
                        self.rls().last_cmd,
                        Some(rl::Complete | rl::CompleteAndSearch)
                    )
                {
                    let (elt, _el) = self.active_edit_line();
                    self.undo(elt);
                    self.update_buff_pos(elt, None);
                }
            }
            rl::RepaintMode | rl::ForceRepaint | rl::Repaint => {
                self.queued_repaint = false;
                self.parser.libdata_mut().is_repaint = true;
                if c == rl::RepaintMode {
                    // Repaint the mode-prompt only if possible.
                    // This is an optimization basically exclusively for vi-mode, since the prompt
                    // may sometimes take a while but when switching the mode all we care about is the
                    // mode-prompt.
                    //
                    // Because some users set `fish_mode_prompt` to an empty function and display the mode
                    // elsewhere, we detect if the mode output is empty.

                    // Don't go into an infinite loop of repainting.
                    // This can happen e.g. if a variable triggers a repaint,
                    // and the variable is set inside the prompt (#7324).
                    // builtin commandline will refuse to enqueue these.
                    self.exec_prompt(false, false);
                    if !self.mode_prompt_buff.is_empty() {
                        if self.is_repaint_needed(None) {
                            self.screen.reset_line(/*repaint_prompt=*/ true);
                            self.layout_and_repaint(L!("mode"));
                        }
                        self.parser.libdata_mut().is_repaint = false;
                        return;
                    }
                    // Else we repaint as normal.
                }
                self.exec_prompt(true, false);
                self.screen.reset_line(/*repaint_prompt=*/ true);
                self.layout_and_repaint(L!("readline"));
                self.force_exec_prompt_and_repaint = false;
                self.parser.libdata_mut().is_repaint = false;
            }
            rl::Complete | rl::CompleteAndSearch => {
                if !self.conf.complete_ok {
                    return;
                }
                if self.is_navigating_pager_contents()
                    || (!self.rls().comp.is_empty()
                        && !self.rls().complete_did_insert
                        && self.rls().last_cmd == Some(rl::Complete))
                {
                    // The user typed complete more than once in a row. If we are not yet fully
                    // disclosed, then become so; otherwise cycle through our available completions.
                    if self.current_page_rendering.remaining_to_disclose != 0 {
                        self.pager.set_fully_disclosed();
                    } else {
                        self.select_completion_in_direction(
                            if c == rl::Complete {
                                SelectionMotion::Next
                            } else {
                                SelectionMotion::Prev
                            },
                            false,
                        );
                    }
                } else {
                    // Either the user hit tab only once, or we had no visible completion list.
                    // Disable tty protocols while we compute completions, so that control-C
                    // triggers SIGINT (suppressed by CSI-U).
                    let mut tty = TtyHandoff::new();
                    tty.disable_tty_protocols();
                    self.compute_and_apply_completions(c);
                    tty.reclaim();
                }
            }
            rl::PagerToggleSearch => {
                if let Some(history_pager) = &self.history_pager {
                    if history_pager.start == 0 {
                        self.flash(0..self.command_line.len());
                        return;
                    }
                    self.fill_history_pager(
                        HistoryPagerInvocation::Advance,
                        Some(SelectionMotion::Next),
                        SearchDirection::Forward,
                    );
                    return;
                }
                if !self.pager.is_empty() {
                    // Toggle search, and begin navigating if we are now searching.
                    let sfs = self.pager.is_search_field_shown();
                    self.pager.set_search_field_shown(!sfs);
                    self.pager.set_fully_disclosed();
                    if self.pager.is_search_field_shown() && !self.is_navigating_pager_contents() {
                        self.select_completion_in_direction(SelectionMotion::South, false);
                    }
                }
            }
            rl::KillLine => {
                let (elt, el) = self.active_edit_line();
                let position = el.position();

                let begin = position;
                let mut end = begin
                    + el.text()[begin..]
                        .chars()
                        .take_while(|&c| c != '\n')
                        .count();

                if end == begin && end < el.len() {
                    end += 1;
                }

                let range = begin..end;
                if !range.is_empty() {
                    self.data.kill(
                        elt,
                        range,
                        Kill::Append,
                        self.rls().last_cmd != Some(rl::KillLine),
                    );
                }
            }
            rl::BackwardKillLine => {
                let (elt, el) = self.active_edit_line();
                let position = el.position();
                if position == 0 {
                    return;
                }
                let text = el.text();

                let end = position;
                let mut begin = position;

                begin -= 1; // make sure we delete at least one character (see issue #580)

                // Delete until we hit a newline, or the beginning of the string.
                while begin != 0 && text.as_char_slice()[begin] != '\n' {
                    begin -= 1;
                }

                // If we landed on a newline, don't delete it.
                if text.as_char_slice()[begin] == '\n' {
                    begin += 1;
                }
                assert!(end >= begin);
                let len = std::cmp::max(end - begin, 1);
                if elt == EditableLineTag::Commandline {
                    self.suppress_autosuggestion = true;
                }
                self.data.kill(
                    elt,
                    end - len..end,
                    Kill::Prepend,
                    self.rls().last_cmd != Some(rl::BackwardKillLine),
                );
            }
            rl::KillWholeLine | rl::KillInnerLine => {
                // The first matches the emacs behavior here: "kills the entire line including
                // the following newline".
                // The second does not kill the following newline
                let (elt, el) = self.active_edit_line();
                let text = el.text();
                let position = el.position();

                // Back up to the character just past the previous newline, or go to the beginning
                // of the command line. Note that if the position is on a newline, visually this
                // looks like the cursor is at the end of a line. Therefore that newline is NOT the
                // beginning of a line; this justifies the -1 check.
                let mut begin = position
                    - text[..position]
                        .chars()
                        .rev()
                        .take_while(|&c| c != '\n')
                        .count();

                // Push end forwards to just past the next newline, or just past the last char.
                let mut end = position;
                loop {
                    if end == text.len() {
                        if c == rl::KillWholeLine && begin > 0 {
                            // We are on the last line. Delete the newline in the beginning to clear
                            // this line.
                            begin -= 1;
                        }
                        break;
                    }
                    if text.as_char_slice()[end] == '\n' {
                        if c == rl::KillWholeLine {
                            end += 1;
                        }
                        break;
                    }
                    end += 1;
                }

                assert!(end >= begin);

                if end > begin {
                    self.data.kill(
                        elt,
                        begin..end,
                        Kill::Append,
                        self.rls().last_cmd != Some(c),
                    );
                }
            }
            rl::Yank => {
                let yank_str = kill_yank();
                self.data
                    .insert_string(self.active_edit_line_tag(), &yank_str);
                self.rls_mut().yank_len = yank_str.len();
                if !yank_str.is_empty() && self.cursor_end_mode == CursorEndMode::Inclusive {
                    let (_elt, el) = self.active_edit_line();
                    self.update_buff_pos(self.active_edit_line_tag(), Some(el.position() - 1));
                }
            }
            rl::YankPop => {
                if self.rls().yank_len != 0 {
                    let (elt, el) = self.active_edit_line();
                    let yank_str = kill_yank_rotate();
                    let new_yank_len = yank_str.len();
                    let bias = if self.cursor_end_mode == CursorEndMode::Inclusive {
                        1
                    } else {
                        0
                    };
                    let begin = el.position() + bias - self.rls().yank_len;
                    let end = el.position() + bias;
                    self.suppress_autosuggestion = true;
                    self.replace_substring(elt, begin..end, yank_str);
                    self.update_buff_pos(elt, None);
                    self.rls_mut().yank_len = new_yank_len;
                }
            }
            rl::BackwardDeleteChar => {
                self.delete_char(true);
            }
            rl::Exit => {
                // This is by definition a successful exit, override the status
                self.parser.set_last_statuses(Statuses::just(STATUS_CMD_OK));
                self.exit_loop_requested = true;
                check_exit_loop_maybe_warning(Some(self));
            }
            rl::DeleteOrExit | rl::DeleteChar => {
                // Remove the current character in the character buffer and on the screen using
                // syntax highlighting, etc.
                let (_elt, el) = self.active_edit_line();
                if el.position() < el.len() {
                    self.delete_char(false);
                } else if c == rl::DeleteOrExit && el.is_empty() {
                    // This is by definition a successful exit, override the status
                    self.parser.set_last_statuses(Statuses::just(STATUS_CMD_OK));
                    self.exit_loop_requested = true;
                    check_exit_loop_maybe_warning(Some(self));
                }
            }
            rl::Execute => {
                if !self.handle_execute() {
                    event::fire_generic(
                        self.parser,
                        L!("fish_posterror").to_owned(),
                        vec![self.command_line.text().to_owned()],
                    );
                    self.screen
                        .reset_abandoning_line(usize::try_from(termsize_last().width).unwrap());
                }
            }
            rl::HistoryPrefixSearchBackward
            | rl::HistoryPrefixSearchForward
            | rl::HistorySearchBackward
            | rl::HistorySearchForward
            | rl::HistoryTokenSearchBackward
            | rl::HistoryTokenSearchForward
            | rl::HistoryLastTokenSearchBackward
            | rl::HistoryLastTokenSearchForward => {
                let mode = match c {
                    rl::HistoryTokenSearchBackward | rl::HistoryTokenSearchForward => {
                        SearchMode::Token
                    }
                    rl::HistoryLastTokenSearchBackward | rl::HistoryLastTokenSearchForward => {
                        SearchMode::LastToken
                    }
                    rl::HistoryPrefixSearchBackward | rl::HistoryPrefixSearchForward => {
                        SearchMode::Prefix
                    }
                    rl::HistorySearchBackward | rl::HistorySearchForward => SearchMode::Line,
                    _ => unreachable!(),
                };

                let was_active_before = self.history_search.active();

                if self.history_search.is_at_present() && mode != self.history_search.mode() {
                    let el = &self.data.command_line;
                    if matches!(mode, SearchMode::Token | SearchMode::LastToken) {
                        // Searching by token.
                        let (token_range, _) = parse_util_token_extent(el.text(), el.position());
                        self.data.history_search.reset_to_mode(
                            el.text()[token_range.clone()].to_owned(),
                            self.history.clone(),
                            mode,
                            token_range.start,
                        );
                    } else {
                        // Searching by line.
                        self.data.history_search.reset_to_mode(
                            el.text().to_owned(),
                            self.history.clone(),
                            mode,
                            0,
                        );

                        // Skip the autosuggestion in the history unless it was truncated.
                        let suggest = &self.data.autosuggestion.text;
                        if !suggest.is_empty()
                            && !self.data.screen.autosuggestion_is_truncated
                            && mode != SearchMode::Prefix
                        {
                            self.data.history_search.add_skip(suggest.clone());
                        }
                    }
                }
                assert!(self.history_search.active());
                let dir = match c {
                    rl::HistorySearchBackward
                    | rl::HistoryTokenSearchBackward
                    | rl::HistoryLastTokenSearchBackward
                    | rl::HistoryPrefixSearchBackward => SearchDirection::Backward,
                    rl::HistorySearchForward
                    | rl::HistoryTokenSearchForward
                    | rl::HistoryLastTokenSearchForward
                    | rl::HistoryPrefixSearchForward => SearchDirection::Forward,
                    _ => unreachable!(),
                };
                let found = self.history_search.move_in_direction(dir);

                // Signal that we've found nothing
                if !found {
                    let result_range = self.history_search.search_result_range();
                    self.flash(if !result_range.is_empty() {
                        result_range
                    } else {
                        0..self.command_line.len()
                    })
                }

                if found {
                    self.update_command_line_from_history_search();
                } else if !was_active_before {
                    self.history_search.reset();
                }
            }
            rl::HistoryPager => {
                if let Some(history_pager) = &self.history_pager {
                    if history_pager.end > self.history.size() {
                        self.flash(0..self.command_line.len());
                        return;
                    }
                    self.fill_history_pager(
                        HistoryPagerInvocation::Advance,
                        Some(SelectionMotion::Next),
                        SearchDirection::Backward,
                    );
                    return;
                }

                // Record our cycle_command_line.
                self.cycle_command_line = self.command_line.text().to_owned();
                self.cycle_cursor_pos = self.command_line.position();

                self.history_pager = Some(0..1);
                // Update the pager data.
                self.pager.set_search_field_shown(true);
                self.pager.set_prefix(
                    if MB_CUR_MAX() > 1 {
                        L!("► ")
                    } else {
                        L!("> ")
                    },
                    /*highlight=*/ false,
                );
                // Update the search field, which triggers the actual history search.
                let search_string = if !self.history_search.active()
                    || self.history_search.search_string().is_empty()
                {
                    let cmdsub = parse_util_cmdsubst_extent(
                        self.command_line.text(),
                        self.command_line.position(),
                    );
                    let cmdsub = &self.command_line.text()[cmdsub];
                    let needle = if !cmdsub.contains('\n') {
                        cmdsub
                    } else {
                        line_at_cursor(self.command_line.text(), self.command_line.position())
                    };
                    parse_util_escape_wildcards(needle)
                } else {
                    // If we have an actual history search already going, reuse that term
                    // - this is if the user looks around a bit and decides to switch to the pager.
                    self.history_search.search_string().to_owned()
                };
                self.insert_string(EditableLineTag::SearchField, &search_string);
            }
            #[allow(deprecated)]
            rl::HistoryDelete | rl::HistoryPagerDelete => {
                // Also applies to ordinary history search.
                let is_history_search = !self.history_search.is_at_present();
                let is_autosuggestion = self.is_at_autosuggestion();
                if is_history_search || is_autosuggestion {
                    self.input_data.function_set_status(true);
                    if is_autosuggestion && !self.autosuggestion.is_whole_item_from_history {
                        self.flash_autosuggestion = true;
                        self.flash(0..0);
                        return;
                    }
                    self.history.remove(if is_history_search {
                        self.history_search.current_result()
                    } else {
                        &self.autosuggestion.text
                    });
                    self.history.save();
                    if is_history_search {
                        self.history_search.handle_deletion();
                        self.update_command_line_from_history_search();
                    } else {
                        self.autosuggestion.clear();
                    }
                    return;
                }
                if self.history_pager.is_none() {
                    self.input_data.function_set_status(false);
                    return;
                }
                self.input_data.function_set_status(true);
                if let Some(completion) =
                    self.pager.selected_completion(&self.current_page_rendering)
                {
                    self.history.remove(&completion.completion);
                    self.history.save();
                    self.fill_history_pager(
                        HistoryPagerInvocation::Refresh,
                        None,
                        SearchDirection::Backward,
                    );
                }
            }
            rl::BackwardChar => {
                let (elt, el) = self.active_edit_line();
                if self.is_navigating_pager_contents() {
                    self.select_completion_in_direction(SelectionMotion::West, false);
                } else if el.position() != 0 {
                    self.update_buff_pos(elt, Some(el.position() - 1));
                }
            }
            rl::BackwardCharPassive => {
                let (elt, el) = self.active_edit_line();
                if el.position() != 0 {
                    if elt == EditableLineTag::SearchField || !self.is_navigating_pager_contents() {
                        self.update_buff_pos(elt, Some(el.position() - 1));
                    }
                }
            }
            rl::ForwardChar | rl::ForwardSingleChar => {
                if self.is_navigating_pager_contents() {
                    self.select_completion_in_direction(SelectionMotion::East, false);
                } else if self.is_at_autosuggestion() {
                    self.accept_autosuggestion(AutosuggestionPortion::Count(
                        if c == rl::ForwardSingleChar {
                            1
                        } else {
                            usize::MAX
                        },
                    ));
                } else if !self.is_at_end() {
                    let (elt, el) = self.active_edit_line();
                    self.update_buff_pos(elt, Some(el.position() + 1));
                }
            }
            rl::ForwardCharPassive => {
                if !self.is_at_end() {
                    let (elt, el) = self.active_edit_line();
                    if elt == EditableLineTag::SearchField || !self.is_navigating_pager_contents() {
                        self.update_buff_pos(elt, Some(el.position() + 1));
                    }
                }
            }
            rl::BackwardKillWord | rl::BackwardKillPathComponent | rl::BackwardKillBigword => {
                let style = match c {
                    rl::BackwardKillBigword => MoveWordStyle::Whitespace,
                    rl::BackwardKillPathComponent => MoveWordStyle::PathComponents,
                    rl::BackwardKillWord => MoveWordStyle::Punctuation,
                    _ => unreachable!(),
                };
                // Is this the same killring item as the last kill?
                let newv = !matches!(
                    self.rls().last_cmd,
                    Some(
                        rl::BackwardKillWord
                            | rl::BackwardKillPathComponent
                            | rl::BackwardKillBigword
                    )
                );
                self.data.move_word(
                    self.active_edit_line_tag(),
                    MoveWordDir::Left,
                    /*erase=*/ true,
                    style,
                    newv,
                )
            }
            rl::KillWord | rl::KillBigword => {
                // The "bigword" functions differ only in that they move to the next whitespace, not
                // punctuation.
                let style = if c == rl::KillWord {
                    MoveWordStyle::Punctuation
                } else {
                    MoveWordStyle::Whitespace
                };
                self.data.move_word(
                    self.active_edit_line_tag(),
                    MoveWordDir::Right,
                    /*erase=*/ true,
                    style,
                    self.rls().last_cmd != Some(c),
                );
            }
            rl::BackwardKillToken => {
                let Some(new_position) = self.backward_token() else {
                    return;
                };

                let (elt, _el) = self.active_edit_line();
                if elt == EditableLineTag::Commandline {
                    self.suppress_autosuggestion = true;
                }

                let (elt, el) = self.active_edit_line();
                self.data.kill(
                    elt,
                    new_position..el.position(),
                    Kill::Prepend,
                    self.rls().last_cmd != Some(rl::BackwardKillToken),
                );
            }
            rl::BackwardToken => {
                let Some(new_position) = self.backward_token() else {
                    return;
                };
                let (elt, _el) = self.active_edit_line();
                self.update_buff_pos(elt, Some(new_position));
            }
            rl::KillToken => {
                let Some(new_position) = self.forward_token(false) else {
                    return;
                };

                let (elt, _el) = self.active_edit_line();
                if elt == EditableLineTag::Commandline {
                    self.suppress_autosuggestion = true;
                }

                let (elt, el) = self.active_edit_line();
                self.data.kill(
                    elt,
                    el.position()..new_position,
                    Kill::Append,
                    self.rls().last_cmd != Some(rl::KillToken),
                );
            }
            rl::ForwardToken => {
                if self.is_at_autosuggestion() {
                    let Some(new_position) = self.forward_token(true) else {
                        return;
                    };
                    let (_elt, el) = self.active_edit_line();
                    let search_string_range = range_of_line_at_cursor(el.text(), el.position());
                    self.accept_autosuggestion(AutosuggestionPortion::Count(
                        new_position - search_string_range.end,
                    ));
                } else if !self.is_at_end() {
                    let Some(new_position) = self.forward_token(false) else {
                        return;
                    };
                    let (elt, _el) = self.active_edit_line();
                    self.update_buff_pos(elt, Some(new_position));
                }
            }
            rl::BackwardWord | rl::BackwardBigword | rl::PrevdOrBackwardWord => {
                if c == rl::PrevdOrBackwardWord && self.command_line.is_empty() {
                    self.eval_bind_cmd(L!("prevd"));
                    self.force_exec_prompt_and_repaint = true;
                    self.input_data
                        .queue_char(CharEvent::from_readline(ReadlineCmd::Repaint));
                    return;
                }

                let style = if c != rl::BackwardBigword {
                    MoveWordStyle::Punctuation
                } else {
                    MoveWordStyle::Whitespace
                };
                self.data.move_word(
                    self.active_edit_line_tag(),
                    MoveWordDir::Left,
                    /*erase=*/ false,
                    style,
                    false,
                );
            }
            rl::ForwardWord | rl::ForwardBigword | rl::NextdOrForwardWord => {
                if c == rl::NextdOrForwardWord && self.command_line.is_empty() {
                    self.eval_bind_cmd(L!("nextd"));
                    self.force_exec_prompt_and_repaint = true;
                    self.input_data
                        .queue_char(CharEvent::from_readline(ReadlineCmd::Repaint));
                    return;
                }

                let style = if c != rl::ForwardBigword {
                    MoveWordStyle::Punctuation
                } else {
                    MoveWordStyle::Whitespace
                };
                if self.is_at_autosuggestion() {
                    self.accept_autosuggestion(AutosuggestionPortion::PerMoveWordStyle(style));
                } else if !self.is_at_end() {
                    let (elt, _el) = self.active_edit_line();
                    self.move_word(elt, MoveWordDir::Right, /*erase=*/ false, style, false);
                }
            }
            rl::BeginningOfHistory | rl::EndOfHistory => {
                let up = c == rl::BeginningOfHistory;
                if self.is_navigating_pager_contents() {
                    self.select_completion_in_direction(
                        if up {
                            SelectionMotion::PageNorth
                        } else {
                            SelectionMotion::PageSouth
                        },
                        false,
                    );
                } else if self.history_search.active() {
                    if up {
                        self.history_search.go_to_oldest();
                    } else {
                        self.history_search.go_to_present();
                    }
                    self.update_command_line_from_history_search();
                }
            }
            rl::UpLine | rl::DownLine => {
                if self.is_navigating_pager_contents() {
                    // We are already navigating pager contents.
                    let direction = if c == rl::DownLine {
                        // Down arrow is always south.
                        SelectionMotion::South
                    } else if self.selection_is_at_top() {
                        // Up arrow, but we are in the first column and first row. End navigation.
                        SelectionMotion::Deselect
                    } else {
                        // Up arrow, go north.
                        SelectionMotion::North
                    };

                    // Now do the selection.
                    self.select_completion_in_direction(direction, false);
                } else if !self.pager.is_empty() {
                    // We pressed a direction with a non-empty pager, begin navigation.
                    self.select_completion_in_direction(
                        if c == rl::DownLine {
                            SelectionMotion::South
                        } else {
                            SelectionMotion::North
                        },
                        false,
                    );
                } else {
                    // Not navigating the pager contents.
                    let (elt, el) = self.active_edit_line();
                    let line_old =
                        i32::try_from(parse_util_get_line_from_offset(el.text(), el.position()))
                            .unwrap();

                    let line_new = if c == rl::UpLine {
                        line_old - 1
                    } else {
                        line_old + 1
                    };

                    let line_count = parse_util_lineno(el.text(), el.len()) - 1;

                    if (0..=i32::try_from(line_count).unwrap()).contains(&line_new) {
                        let indents = parse_util_compute_indents(el.text());
                        let base_pos_new =
                            parse_util_get_offset_from_line(el.text(), line_new).unwrap();
                        let base_pos_old =
                            parse_util_get_offset_from_line(el.text(), line_old).unwrap();

                        let indent_old = indents[std::cmp::min(indents.len() - 1, base_pos_old)];
                        let indent_new = indents[std::cmp::min(indents.len() - 1, base_pos_new)];
                        let indent_old = isize::try_from(indent_old).unwrap();
                        let indent_new = isize::try_from(indent_new).unwrap();

                        let line_offset_old =
                            isize::try_from(el.position() - base_pos_old).unwrap();
                        let total_offset_new = parse_util_get_offset(
                            el.text(),
                            line_new,
                            line_offset_old
                                - isize::try_from(SPACES_PER_INDENT).unwrap()
                                    * (indent_new - indent_old),
                        );
                        self.update_buff_pos(elt, total_offset_new);
                    }
                }
            }
            rl::SuppressAutosuggestion => {
                self.suppress_autosuggestion = true;
                let success = self.is_at_line_with_autosuggestion();
                self.autosuggestion.clear();
                // Return true if we had a suggestion to clear.
                self.input_data.function_set_status(success);
            }
            rl::AcceptAutosuggestion => {
                let success = self.is_at_line_with_autosuggestion();
                if success {
                    self.accept_autosuggestion(AutosuggestionPortion::Count(usize::MAX));
                }
                self.input_data.function_set_status(success);
            }
            rl::TransposeChars => {
                let (elt, el) = self.active_edit_line();
                if el.len() < 2 {
                    return;
                }

                // If the cursor is at the end, transpose the last two characters of the line.
                if el.position() == el.len() {
                    self.update_buff_pos(elt, Some(el.position() - 1));
                }

                // Drag the character before the cursor forward over the character at the cursor,
                // moving the cursor forward as well.
                let (elt, el) = self.active_edit_line();
                if el.position() > 0 {
                    let mut local_cmd = el.text().to_owned();
                    local_cmd
                        .as_char_slice_mut()
                        .swap(el.position(), el.position() - 1);
                    self.data
                        .set_command_line_and_position(elt, local_cmd, el.position() + 1);
                }
            }
            rl::TransposeWords => {
                let (elt, el) = self.active_edit_line();

                // If we are not in a token, look for one ahead.
                let buff_pos = el.position()
                    + el.text()[el.position()..]
                        .chars()
                        .take_while(|c| !c.is_alphanumeric())
                        .count();

                self.update_buff_pos(elt, Some(buff_pos));
                let (elt, el) = self.active_edit_line();
                let text = el.text();

                let (mut tok, mut prev_tok) = parse_util_token_extent(text, el.position());

                // In case we didn't find a token at or after the cursor...
                if tok.start == el.len() {
                    // ...retry beginning from the previous token.
                    let pos = prev_tok.end;
                    (tok, prev_tok) = parse_util_token_extent(text, pos);
                }

                // Make sure we have two tokens.
                if !prev_tok.is_empty() && !tok.is_empty() && tok.start > prev_tok.start {
                    let prev = &text[prev_tok.clone()];
                    let sep = &text[prev_tok.end..tok.start];
                    let trail = &text[tok.end..];

                    // Compose new command line with swapped tokens.
                    let mut new_text = text[..prev_tok.start].to_owned();
                    new_text.push_utfstr(&text[tok.clone()]);
                    new_text.push_utfstr(sep);
                    new_text.push_utfstr(prev);
                    new_text.push_utfstr(trail);
                    // Put cursor right after the second token.
                    self.set_command_line_and_position(elt, new_text, tok.end);
                }
            }
            rl::TogglecaseChar => {
                let (elt, el) = self.active_edit_line();
                let buff_pos = el.position();

                // Check that the cursor is on a character
                if buff_pos != el.len() {
                    let chr = el.text().as_char_slice()[buff_pos];

                    // Toggle the case of the current character
                    let make_uppercase = chr.is_lowercase();
                    let replacement = if make_uppercase {
                        WString::from_iter(chr.to_uppercase())
                    } else {
                        WString::from_iter(chr.to_lowercase())
                    };

                    self.replace_substring(elt, buff_pos..buff_pos + 1, replacement);

                    // Restore the buffer position since replace_substring moves
                    // the buffer position ahead of the replaced text.
                    self.update_buff_pos(elt, Some(buff_pos));
                }
            }
            rl::TogglecaseSelection => {
                let (elt, el) = self.active_edit_line();

                // Check that we have an active selection and get the bounds.
                if let Some(selection) = self.get_selection() {
                    let mut replacement = WString::new();

                    // Loop through the selected characters and toggle their case.
                    for pos in selection.clone() {
                        if pos >= el.len() {
                            break;
                        }
                        let chr = el.text().as_char_slice()[pos];

                        // Toggle the case of the current character.
                        let make_uppercase = chr.is_lowercase();
                        if make_uppercase {
                            replacement.extend(chr.to_uppercase());
                        } else {
                            replacement.extend(chr.to_lowercase());
                        }
                    }

                    let buff_pos = el.position();
                    self.replace_substring(elt, selection, replacement);

                    // Restore the buffer position since replace_substring moves
                    // the buffer position ahead of the replaced text.
                    self.update_buff_pos(elt, Some(buff_pos));
                }
            }
            rl::UpcaseSelection | rl::DowncaseSelection => {
                let (elt, el) = self.active_edit_line();

                // Check that we have an active selection and get the bounds.
                if let Some(selection) = self.get_selection() {
                    let text = &el.text().as_char_slice()[selection.clone()];
                    let replacement = if c == rl::UpcaseSelection {
                        WString::from_iter(text.iter().flat_map(|c| c.to_uppercase()))
                    } else {
                        WString::from_iter(text.iter().flat_map(|c| c.to_lowercase()))
                    };

                    let buff_pos = el.position();
                    self.replace_substring(elt, selection, replacement);

                    // Restore the buffer position since replace_substring moves
                    // the buffer position ahead of the replaced text.
                    // Note: This does not take string length changes into account.
                    // E.g.: When the cursor was at the right of the selection,
                    // the selection contains 'ẞ', which is uppercased into 'SS',
                    // the cursor will stay at the same offset, but it will not be on the same
                    // character as before.
                    // The position calculations work on codepoints rather than graphemes, which can
                    // result in additional issues.
                    self.update_buff_pos(elt, Some(buff_pos));
                }
            }
            rl::UpcaseWord | rl::DowncaseWord | rl::CapitalizeWord => {
                let (elt, el) = self.active_edit_line();
                // For capitalize_word, whether we've capitalized a character so far.
                let mut capitalized_first = false;

                // We apply the operation from the current location to the end of the word.
                let mut pos = el.position();
                let init_pos = pos;
                self.move_word(
                    elt,
                    MoveWordDir::Right,
                    false,
                    MoveWordStyle::Punctuation,
                    false,
                );
                let (elt, el) = self.active_edit_line();
                let mut replacement = WString::new();
                while pos
                    < if self.cursor_selection_mode == CursorSelectionMode::Inclusive
                        && self.is_at_end()
                    {
                        el.len()
                    } else {
                        el.position()
                    }
                {
                    let chr = el.text().as_char_slice()[pos];

                    // We always change the case; this decides whether we go uppercase (true) or
                    // lowercase (false).
                    let make_uppercase = if c == rl::CapitalizeWord {
                        !capitalized_first && chr.is_alphanumeric()
                    } else {
                        c == rl::UpcaseWord
                    };

                    // Apply the operation and then record what we did.
                    if make_uppercase {
                        replacement.extend(chr.to_uppercase());
                    } else {
                        replacement.extend(chr.to_lowercase());
                    };
                    capitalized_first = capitalized_first || make_uppercase;
                    pos += 1;
                }
                self.replace_substring(elt, init_pos..pos, replacement);
                self.update_buff_pos(elt, None);
            }
            rl::BeginSelection => {
                let mut selection = SelectionData::default();
                let pos = self.command_line.position();
                selection.begin = pos;
                selection.start = pos;
                selection.stop = pos
                    + if self.cursor_selection_mode == CursorSelectionMode::Inclusive {
                        1
                    } else {
                        0
                    };
                self.selection = Some(selection);
            }
            rl::EndSelection => {
                self.selection = None;
            }
            rl::SwapSelectionStartStop => {
                let position = self.command_line.position();
                let Some(selection) = &mut self.selection else {
                    return;
                };
                let tmp = selection.begin;
                selection.begin = position;
                selection.start = position;
                self.update_buff_pos(self.active_edit_line_tag(), Some(tmp));
            }
            rl::KillSelection => {
                let newv = self.rls().last_cmd != Some(rl::KillSelection);
                if let Some(selection) = self.get_selection() {
                    self.kill(EditableLineTag::Commandline, selection, Kill::Append, newv);
                }
            }
            rl::InsertLineOver => {
                let elt = loop {
                    let (elt, el) = self.active_edit_line();
                    if el.position() == 0 || el.text().as_char_slice()[el.position() - 1] == '\n' {
                        break elt;
                    }
                    if !self.update_buff_pos(elt, Some(el.position() - 1)) {
                        break elt;
                    }
                };
                self.insert_char(elt, '\n');
                let (elt, el) = self.active_edit_line();
                self.update_buff_pos(elt, Some(el.position() - 1));
            }
            rl::InsertLineUnder => {
                let elt = loop {
                    let (elt, el) = self.active_edit_line();
                    if el.position() == el.len() || el.text().as_char_slice()[el.position()] == '\n'
                    {
                        break elt;
                    }
                    if !self.update_buff_pos(elt, Some(el.position() + 1)) {
                        break elt;
                    }
                };
                self.insert_char(elt, '\n');
            }
            rl::ForwardJump | rl::BackwardJump | rl::ForwardJumpTill | rl::BackwardJumpTill => {
                let direction = match c {
                    rl::ForwardJump | rl::ForwardJumpTill => JumpDirection::Forward,
                    rl::BackwardJump | rl::BackwardJumpTill => JumpDirection::Backward,
                    _ => unreachable!(),
                };
                let precision = match c {
                    rl::ForwardJump | rl::BackwardJump => JumpPrecision::To,
                    rl::ForwardJumpTill | rl::BackwardJumpTill => JumpPrecision::Till,
                    _ => unreachable!(),
                };
                let (elt, _el) = self.active_edit_line();
                if let Some(target) = self.function_pop_arg() {
                    let success =
                        self.jump_and_remember_last_jump(direction, precision, elt, target, false);

                    self.input_data.function_set_status(success);
                }
            }
            rl::JumpToMatchingBracket | rl::JumpTillMatchingBracket => {
                let (elt, _el) = self.active_edit_line();
                let el = self.edit_line(elt);
                let l_brackets = ['(', '[', '{'];
                let r_brackets = [')', ']', '}'];
                let cursor = el.position();
                let precision = match c {
                    rl::JumpToMatchingBracket => JumpPrecision::To,
                    rl::JumpTillMatchingBracket => JumpPrecision::Till,
                    _ => unreachable!(),
                };
                let jump_from_pos = match c {
                    _ if l_brackets.contains(&el.at(cursor))
                        || r_brackets.contains(&el.at(cursor)) =>
                    {
                        Some(cursor)
                    }
                    rl::JumpTillMatchingBracket
                        if cursor > 0 && l_brackets.contains(&el.at(cursor - 1)) =>
                    {
                        Some(cursor - 1)
                    }
                    rl::JumpTillMatchingBracket
                        if cursor < el.len() && r_brackets.contains(&el.at(cursor + 1)) =>
                    {
                        Some(cursor + 1)
                    }
                    _ => None,
                };
                let success = match jump_from_pos {
                    Some(jump_from_pos) => {
                        let l_bracket = match el.at(jump_from_pos) {
                            '(' | ')' => '(',
                            '[' | ']' => '[',
                            '{' | '}' => '{',
                            _ => unreachable!(),
                        };
                        let r_bracket = match l_bracket {
                            '(' => ')',
                            '[' => ']',
                            '{' => '}',
                            _ => unreachable!(),
                        };
                        self.jump_to_matching_bracket(
                            precision,
                            elt,
                            jump_from_pos,
                            l_bracket,
                            r_bracket,
                        )
                    }
                    // If we stand on non-bracket character, we prefer to jump forward
                    None => self.jump(
                        JumpDirection::Forward,
                        precision,
                        elt,
                        r_brackets.to_vec(),
                        false,
                    ),
                };
                self.input_data.function_set_status(success);
            }
            rl::RepeatJump => {
                let (elt, _el) = self.active_edit_line();
                let mut success = false;

                if let Some(target) = self.last_jump_target {
                    success = self.data.jump_and_remember_last_jump(
                        self.data.last_jump_direction,
                        self.data.last_jump_precision,
                        elt,
                        target,
                        true,
                    );
                }

                self.input_data.function_set_status(success);
            }
            rl::ReverseRepeatJump => {
                let (elt, _el) = self.active_edit_line();
                let mut success = false;
                let original_dir = self.last_jump_direction;

                let dir = if self.last_jump_direction == JumpDirection::Forward {
                    JumpDirection::Backward
                } else {
                    JumpDirection::Forward
                };

                if let Some(last_target) = self.last_jump_target {
                    success = self.data.jump_and_remember_last_jump(
                        dir,
                        self.data.last_jump_precision,
                        elt,
                        last_target,
                        true,
                    );
                }

                self.last_jump_direction = original_dir;

                self.input_data.function_set_status(success);
            }
            rl::ExpandAbbr => {
                if self.expand_abbreviation_at_cursor(1) {
                    self.input_data.function_set_status(true);
                } else {
                    self.input_data.function_set_status(false);
                }
            }
            rl::Undo | rl::Redo => {
                let (elt, _el) = self.active_edit_line();
                let ok = if c == rl::Undo {
                    self.undo(elt)
                } else {
                    self.redo(elt)
                };
                if !ok {
                    self.flash(0..self.command_line.len());
                    return;
                }
                self.suppress_autosuggestion = false;
                if elt == EditableLineTag::Commandline {
                    self.clear_pager();
                }
                self.update_buff_pos(elt, None);
            }
            rl::BeginUndoGroup => {
                let (_elt, el) = self.active_edit_line_mut();
                el.begin_edit_group();
            }
            rl::EndUndoGroup => {
                let (_elt, el) = self.active_edit_line_mut();
                el.end_edit_group();
            }
            rl::ClearScreenAndRepaint => {
                self.clear_screen_and_repaint();
            }
            rl::ScrollbackPush => {
                if !SCROLL_FORWARD_SUPPORTED.load() {
                    return;
                }
                let query = self.blocking_query();
                let Some(query) = &*query else {
                    drop(query);
                    self.request_cursor_position(
                        &mut Outputter::stdoutput().borrow_mut(),
                        CursorPositionQuery::ScrollbackPush,
                    );
                    return;
                };
                match query {
                    TerminalQuery::PrimaryDeviceAttribute => panic!(),
                    TerminalQuery::CursorPositionReport(_) => {
                        // TODO: re-queue it I guess.
                        FLOG!(
                            reader,
                            "Ignoring scrollback-push received while still waiting for Cursor Position Report"
                        );
                    }
                }
            }
            rl::SelfInsert | rl::SelfInsertNotFirst | rl::FuncAnd | rl::FuncOr => {
                // This can be reached via `commandline -f and` etc
                // panic!("should have been handled by inputter_t::readch");
            }
        }
    }

    fn clear_screen_and_repaint(&mut self) {
        self.parser.libdata_mut().is_repaint = true;

        // Clear the screen.
        // This is subtle: We first clear, draw the old prompt,
        // and *then* reexecute the prompt and overdraw it.
        // This removes the flicker,
        // while keeping the prompt up-to-date.
        Outputter::stdoutput()
            .borrow_mut()
            .write_command(ClearScreen);
        self.screen.reset_line(/*repaint_prompt=*/ true);
        self.layout_and_repaint(L!("readline"));

        self.exec_prompt(true, false);
        self.screen.reset_line(/*repaint_prompt=*/ true);
        self.layout_and_repaint(L!("readline"));
        self.force_exec_prompt_and_repaint = false;
        self.parser.libdata_mut().is_repaint = false;
    }

    fn backward_token(&mut self) -> Option<usize> {
        let (_elt, el) = self.active_edit_line();
        let pos = el.position();
        if pos == 0 {
            return None;
        }

        let (tok, prev_tok) = parse_util_token_extent(el.text(), el.position());

        // if we are at the start of a token, go back one
        let new_position = if tok.start == pos {
            if prev_tok.start == pos {
                let cmdsub = parse_util_cmdsubst_extent(el.text(), prev_tok.start);
                cmdsub.start.saturating_sub(1)
            } else {
                prev_tok.start
            }
        } else {
            tok.start
        };

        Some(new_position)
    }

    fn forward_token(&self, autosuggest: bool) -> Option<usize> {
        let (elt, el) = self.active_edit_line();
        let pos = el.position();
        let buffer = if autosuggest {
            assert!(elt == EditableLineTag::Commandline);
            assert!(self.is_at_line_with_autosuggestion());
            let autosuggestion = &self.autosuggestion;
            Cow::Owned(combine_command_and_autosuggestion(
                el.text(),
                autosuggestion.search_string_range.clone(),
                &autosuggestion.text,
            ))
        } else {
            Cow::Borrowed(el.text())
        };
        if pos == buffer.len() {
            return None;
        }

        let cmdsubst_range = parse_util_cmdsubst_extent(&buffer, pos);
        for token in Tokenizer::new(&buffer[cmdsubst_range.clone()], TOK_ACCEPT_UNFINISHED) {
            if token.type_ != TokenType::string {
                continue;
            }
            let tok_end = cmdsubst_range.start + token.end();
            if tok_end > pos {
                return Some(tok_end);
            }
        }
        Some(el.len())
    }
}

/// Returns true if the last token is a comment.
fn text_ends_in_comment(text: &wstr) -> bool {
    Tokenizer::new(text, TOK_ACCEPT_UNFINISHED | TOK_SHOW_COMMENTS)
        .last()
        .is_some_and(|token| token.type_ == TokenType::comment)
}

impl<'a> Reader<'a> {
    // Handle readline_cmd_t::execute. This may mean inserting a newline if the command is
    // unfinished. It may also set 'finished' and 'cmd' inside the rls.
    // Return true on success, false if we got an error, in which case the caller should fire the
    // error event.
    fn handle_execute(&mut self) -> bool {
        // Evaluate. If the current command is unfinished, or if the character is escaped
        // using a backslash, insert a newline.
        // If the user hits return while navigating the pager, it only clears the pager.
        if self.is_navigating_pager_contents() {
            let search_field = &self.data.pager.search_field_line;
            if self.history_pager.is_some() && self.pager.selected_completion_idx.is_none() {
                let range = 0..self.command_line.len();
                let offset_from_end = search_field.len() - search_field.position();
                let mut cursor = self.command_line.position();
                let updated = replace_line_at_cursor(
                    self.command_line.text(),
                    &mut cursor,
                    search_field.text(),
                );
                self.replace_substring(EditableLineTag::Commandline, range, updated);
                self.command_line.set_position(cursor - offset_from_end);
            } else if self
                .pager
                .selected_completion(&self.data.current_page_rendering)
                .is_none()
            {
                let failed_search = search_field.text().to_owned();
                self.insert_string(EditableLineTag::Commandline, &failed_search);
            }
            self.clear_pager();
            return true;
        }

        // Delete any autosuggestion.
        self.autosuggestion.clear();

        // The user may have hit return with pager contents, but while not navigating them.
        // Clear the pager in that event.
        self.clear_pager();

        // We only execute the command line.
        let elt = EditableLineTag::Commandline;
        let el = &mut self.command_line;

        // Allow backslash-escaped newlines.
        let mut continue_on_next_line = false;
        if el.position() >= el.len() {
            // We're at the end of the text and not in a comment (issue #1225).
            continue_on_next_line =
                is_backslashed(el.text(), el.position()) && !text_ends_in_comment(el.text());
        } else {
            // Allow mid line split if the following character is whitespace (issue #613).
            if is_backslashed(el.text(), el.position())
                && el.text().as_char_slice()[el.position()].is_whitespace()
            {
                continue_on_next_line = true;
                // Check if the end of the line is backslashed (issue #4467).
            } else if is_backslashed(el.text(), el.len()) && !text_ends_in_comment(el.text()) {
                // Move the cursor to the end of the line.
                el.set_position(el.len());
                continue_on_next_line = true;
            }
        }
        // If the conditions are met, insert a new line at the position of the cursor.
        if continue_on_next_line {
            self.insert_char(elt, '\n');
            return true;
        }

        // Expand the command line in preparation for execution.
        // to_exec is the command to execute; the command line itself has the command for history.
        let test_res = self.expand_for_execute();
        if let Err(err) = test_res {
            if err.contains(ParserTestErrorBits::ERROR) {
                return false;
            } else if err.contains(ParserTestErrorBits::INCOMPLETE) {
                self.insert_char(elt, '\n');
                return true;
            }
            unreachable!();
        }

        self.add_to_history();
        self.rls_mut().finished = true;
        self.command_line.pending_position = Some(self.command_line.position());
        self.update_buff_pos(elt, Some(self.command_line_len()));
        true
    }

    // Expand abbreviations before execution.
    // Replace the command line with any abbreviations as needed.
    // Return the test result, which may be incomplete to insert a newline, or an error.
    fn expand_for_execute(&mut self) -> Result<(), ParserTestErrorBits> {
        // Expand abbreviations at the cursor.
        // The first expansion is "user visible" and enters into history.
        let el = &self.command_line;

        let mut test_res = Ok(());

        // Syntax check before expanding abbreviations. We could consider relaxing this: a string may be
        // syntactically invalid but become valid after expanding abbreviations.
        if self.conf.syntax_check_ok {
            test_res = reader_shell_test(self.parser, el.text());
            if test_res.is_err_and(|err| err.contains(ParserTestErrorBits::ERROR)) {
                return test_res;
            }
        }

        // Exec abbreviations at the cursor.
        // Note we want to expand abbreviations even if incomplete.
        if self.expand_abbreviation_at_cursor(0) {
            // Trigger syntax highlighting as we are likely about to execute this command.
            self.super_highlight_me_plenty();
            if self.conf.syntax_check_ok {
                let el = &self.command_line;
                test_res = reader_shell_test(self.parser, el.text());
            }
        }
        test_res
    }
}

impl ReaderData {
    // Ensure we have no pager contents.
    fn clear_pager(&mut self) {
        self.pager.clear();
        self.history_pager = None;
        self.clear(EditableLineTag::SearchField);
        self.command_line_transient_edit = None;
    }

    fn get_selection(&self) -> Option<Range<usize>> {
        let selection = self.selection?;
        let start = std::cmp::min(selection.start, self.command_line.len());
        let end = std::cmp::min(selection.stop, self.command_line.len());
        if start == end {
            return None;
        }
        Some(start..end)
    }

    fn selection_is_at_top(&self) -> bool {
        let pager = &self.pager;
        let row = pager.get_selected_row(&self.current_page_rendering);
        if row.is_some_and(|row| row != 0) {
            return false;
        }

        let col = pager.get_selected_column(&self.current_page_rendering);
        !col.is_some_and(|col| col != 0)
    }
}

impl<'a> Reader<'a> {
    /// Called to update the termsize, including $COLUMNS and $LINES, as necessary.
    fn update_termsize(&mut self) {
        termsize_update(self.parser);
    }

    /// Flash the screen. This function changes the color of the current line momentarily.
    fn flash(&mut self, mut flash_range: Range<usize>) {
        // Multiple flashes may be enqueued by keypress repeat events and can pile up to cause a
        // significant delay in processing future input while all the flash() calls complete, as we
        // effectively sleep for 100ms each go. See #8610.
        let now = Instant::now();
        if self
            .last_flash
            .is_some_and(|last_flash| now.duration_since(last_flash) < Duration::from_millis(50))
            || flash_range.is_empty() && !self.flash_autosuggestion
        {
            self.last_flash = Some(now);
            return;
        }

        let mut data = self.make_layout_data();

        // Save off the colors and set the background.
        let saved_colors = data.colors.clone();
        if flash_range.end > data.colors.len() {
            flash_range.start = flash_range.start.min(data.colors.len());
            flash_range.end = data.colors.len();
        }
        for color in &mut data.colors[flash_range] {
            color.foreground = HighlightRole::search_match;
            color.background = HighlightRole::search_match;
        }
        self.rendered_layout = data;
        self.paint_layout(L!("flash"), false);

        self.flash_autosuggestion = false;
        std::thread::sleep(Duration::from_millis(100));

        // Re-render with our saved data.
        self.rendered_layout.colors = saved_colors;
        self.paint_layout(L!("unflash"), false);

        // Save the time we stopped flashing as the time of the most recent flash. We can't just
        // increment the old `now` value because the sleep is non-deterministic.
        self.last_flash = Some(Instant::now());
    }
}

impl ReaderData {
    /// Do what we need to do whenever our pager selection changes.
    fn pager_selection_changed(&mut self) {
        assert_is_main_thread();

        // Update the cursor and command line.
        let mut cursor_pos = self.cycle_cursor_pos;

        if let Some(transient_edit) = self.command_line_transient_edit.take() {
            if transient_edit == TransientEdit::Pager {
                self.undo(EditableLineTag::Commandline);
            }
        }

        if let Some(completion) = self.pager.selected_completion(&self.current_page_rendering) {
            let new_cmd_line = completion_apply_to_command_line(
                &OperationContext::background_interruptible(EnvStack::globals()), // To-do: include locals.
                &completion.completion,
                completion.flags,
                &self.cycle_command_line,
                &mut cursor_pos,
                false,
                /*is_unique=*/ false, // don't care
            );
            // Only update if something changed, to avoid useless edits in the undo history.
            if new_cmd_line != self.command_line.text() && new_cmd_line != self.cycle_command_line {
                self.set_buffer_maintaining_pager(&new_cmd_line, cursor_pos);
                self.command_line_transient_edit = Some(TransientEdit::Pager);
            }
        } else {
            self.update_buff_pos(EditableLineTag::Commandline, None);
        }
    }

    /// Sets the command line contents, without clearing the pager.
    fn set_buffer_maintaining_pager(&mut self, new_cmd_line: &wstr, pos: usize) {
        self.replace_substring(
            EditableLineTag::Commandline,
            0..self.command_line.len(),
            new_cmd_line.to_owned(),
        );

        // Don't set a position past the command line length.
        self.update_buff_pos(
            EditableLineTag::Commandline,
            Some(pos.min(new_cmd_line.len())),
        );

        // Clear history search.
        self.history_search.reset();
    }

    fn select_completion_in_direction(
        &mut self,
        dir: SelectionMotion,
        force_selection_change: bool, /* = false */
    ) {
        let selection_changed = self
            .pager
            .select_next_completion_in_direction(dir, &self.current_page_rendering);
        if force_selection_change || selection_changed {
            self.pager_selection_changed();
        }
    }
}

/// Restore terminal settings we care about, to prevent a broken shell.
fn term_fix_modes(modes: &mut libc::termios) {
    modes.c_iflag &= !ICRNL; // disable mapping CR (\cM) to NL (\cJ)
    modes.c_iflag &= !INLCR; // disable mapping NL (\cJ) to CR (\cM)
    modes.c_lflag &= !ICANON; // turn off canonical mode
    modes.c_lflag &= !ECHO; // turn off echo mode
    modes.c_lflag &= !IEXTEN; // turn off handling of discard and lnext characters
    modes.c_oflag |= OPOST; // turn on "implementation-defined post processing" - this often
                            // changes how line breaks work.
    modes.c_oflag |= ONLCR; // "translate newline to carriage return-newline" - without
                            // you see staircase output.

    modes.c_cc[VMIN] = 1;
    modes.c_cc[VTIME] = 0;

    // Prefer to use _POSIX_VDISABLE to disable control functions.
    // This permits separately binding nul (typically control-space).
    // POSIX calls out -1 as a special value which should be ignored.
    let disabling_char = _POSIX_VDISABLE;

    // We ignore these anyway, so there is no need to sacrifice a character.
    modes.c_cc[VSUSP] = disabling_char;
    modes.c_cc[VQUIT] = disabling_char;
}

fn term_fix_external_modes(modes: &mut libc::termios) {
    // Turning off OPOST or ONLCR breaks output (staircase effect), we don't allow it.
    // See #7133.
    modes.c_oflag |= OPOST;
    modes.c_oflag |= ONLCR;
    // These cause other ridiculous behaviors like input not being shown.
    modes.c_lflag |= ICANON;
    modes.c_lflag |= IEXTEN;
    modes.c_lflag |= ECHO;
    modes.c_iflag |= ICRNL;
    modes.c_iflag &= !INLCR;
}

/// Give up control of terminal.
fn term_donate(quiet: bool /* = false */) {
    while unsafe {
        libc::tcsetattr(
            STDIN_FILENO,
            TCSANOW,
            &*TTY_MODES_FOR_EXTERNAL_CMDS.lock().unwrap(),
        )
    } == -1
    {
        if errno().0 != EINTR {
            if !quiet {
                FLOG!(
                    warning,
                    wgettext!("Could not set terminal mode for new job")
                );
                perror("tcsetattr");
            }
            break;
        }
    }
}

/// Copy the (potentially changed) terminal modes and use them from now on.
pub fn term_copy_modes() {
    let mut modes = MaybeUninit::uninit();
    unsafe { libc::tcgetattr(STDIN_FILENO, modes.as_mut_ptr()) };
    let mut tty_modes_for_external_cmds = TTY_MODES_FOR_EXTERNAL_CMDS.lock().unwrap();
    *tty_modes_for_external_cmds = unsafe { modes.assume_init() };
    // We still want to fix most egregious breakage.
    // E.g. OPOST is *not* something that should be set globally,
    // and 99% triggered by a crashed program.
    term_fix_external_modes(&mut tty_modes_for_external_cmds);

    // Copy flow control settings to shell modes.
    if (tty_modes_for_external_cmds.c_iflag & IXON) != 0 {
        shell_modes().c_iflag |= IXON;
    } else {
        shell_modes().c_iflag &= !IXON;
    }
    if (tty_modes_for_external_cmds.c_iflag & IXOFF) != 0 {
        shell_modes().c_iflag |= IXOFF;
    } else {
        shell_modes().c_iflag &= !IXOFF;
    }
}

/// Grab control of terminal.
fn term_steal(copy_modes: bool) {
    if copy_modes {
        term_copy_modes();
    }
    while unsafe { libc::tcsetattr(STDIN_FILENO, TCSANOW, &*shell_modes()) } == -1 {
        if errno().0 != EINTR {
            FLOG!(warning, wgettext!("Could not set terminal mode for shell"));
            perror("tcsetattr");
            break;
        }
    }

    termsize_invalidate_tty();
}

// Ensure that fish owns the terminal, possibly waiting. If we cannot acquire the terminal, then
// report an error and exit.
fn acquire_tty_or_exit(shell_pgid: libc::pid_t) {
    assert_is_main_thread();

    // Check if we are in control of the terminal, so that we don't do semi-expensive things like
    // reset signal handlers unless we really have to, which we often don't.
    // Common case.
    let mut owner = unsafe { libc::tcgetpgrp(STDIN_FILENO) };
    if owner == shell_pgid {
        return;
    }

    // In some strange cases the tty may be come preassigned to fish's pid, but not its pgroup.
    // In that case we simply attempt to claim our own pgroup.
    // See #7388.
    if owner == getpid() {
        unsafe { libc::setpgid(owner, owner) };
        return;
    }

    // Bummer, we are not in control of the terminal. Stop until parent has given us control of
    // it.
    //
    // In theory, resetting signal handlers could cause us to miss signal deliveries. In
    // practice, this code should only be run during startup, when we're not waiting for any
    // signals.
    signal_reset_handlers();
    let _restore_sigs = ScopeGuard::new((), |()| signal_set_handlers(true));

    // Ok, signal handlers are taken out of the picture. Stop ourself in a loop until we are in
    // control of the terminal. However, the call to signal(SIGTTIN) may silently not do
    // anything if we are orphaned.
    //
    // As far as I can tell there's no really good way to detect that we are orphaned. One way
    // is to just detect if the group leader exited, via kill(shell_pgid, 0). Another
    // possibility is that read() from the tty fails with EIO - this is more reliable but it's
    // harder, because it may succeed or block. So we loop for a while, trying those strategies.
    // Eventually we just give up and assume we're orphaend.
    for loop_count in 0.. {
        owner = unsafe { libc::tcgetpgrp(STDIN_FILENO) };
        // 0 is a valid return code from `tcgetpgrp()` under at least FreeBSD and testing
        // indicates that a subsequent call to `tcsetpgrp()` will succeed. 0 is the
        // pid of the top-level kernel process, so I'm not sure if this means ownership
        // of the terminal has gone back to the kernel (i.e. it's not owned) or if it is
        // just an "invalid" pid for all intents and purposes.
        if owner == 0 {
            unsafe { libc::tcsetpgrp(STDIN_FILENO, shell_pgid) };
            // Since we expect the above to work, call `tcgetpgrp()` immediately to
            // avoid a second pass through this loop.
            owner = unsafe { libc::tcgetpgrp(STDIN_FILENO) };
        }
        if owner == -1 && errno().0 == ENOTTY {
            if !is_interactive_session() {
                // It's OK if we're not able to take control of the terminal. We handle
                // the fallout from this in a few other places.
                break;
            }
            // No TTY, cannot be interactive?
            FLOG!(
                warning,
                wgettext!("No TTY for interactive shell (tcgetpgrp failed)")
            );
            perror("setpgid");
            exit_without_destructors(1);
        }
        if owner == shell_pgid {
            break; // success
        } else {
            if check_for_orphaned_process(loop_count, shell_pgid) {
                // We're orphaned, so we just die. Another sad statistic.
                let pid = getpid();
                FLOG!(warning, wgettext_fmt!("I appear to be an orphaned process, so I am quitting politely. My pid is %d.", pid));
                exit_without_destructors(1);
            }

            // Try stopping us.
            let ret = unsafe { libc::killpg(shell_pgid, SIGTTIN) };
            if ret < 0 {
                perror("killpg(shell_pgid, SIGTTIN)");
                exit_without_destructors(1);
            }
        }
    }
}

/// Initialize data for interactive use.
fn reader_interactive_init(parser: &Parser) {
    assert_is_main_thread();

    let mut shell_pgid = getpgrp();
    let shell_pid = getpid();

    // Set up key bindings.
    init_input();

    // Ensure interactive signal handling is enabled.
    signal_set_handlers_once(true);

    // Wait until we own the terminal.
    acquire_tty_or_exit(shell_pgid);

    // If fish has no valid pgroup (possible with firejail, see #5295) or is interactive,
    // ensure it owns the terminal. Also see #5909, #7060.
    if shell_pgid == 0 || (is_interactive_session() && shell_pgid != shell_pid) {
        shell_pgid = shell_pid;
        if unsafe { libc::setpgid(shell_pgid, shell_pgid) } < 0 {
            // If we're session leader setpgid returns EPERM. The other cases where we'd get EPERM
            // don't apply as we passed our own pid.
            //
            // This should be harmless, so we ignore it.
            if errno().0 != EPERM {
                FLOG!(
                    error,
                    wgettext!("Failed to assign shell to its own process group")
                );
                perror("setpgid");
                exit_without_destructors(1);
            }
        }

        // Take control of the terminal
        if unsafe { libc::tcsetpgrp(STDIN_FILENO, shell_pgid) } == -1 {
            FLOG!(error, wgettext!("Failed to take control of the terminal"));
            perror("tcsetpgrp");
            exit_without_destructors(1);
        }

        // Configure terminal attributes
        if unsafe { libc::tcsetattr(STDIN_FILENO, TCSANOW, &*shell_modes()) } == -1 {
            FLOG!(warning, wgettext!("Failed to set startup terminal mode!"));
            perror("tcsetattr");
        }
    }

    termsize_invalidate_tty();

    // Provide value for `status current-command`
    parser.libdata_mut().status_vars.command = L!("fish").to_owned();
    // Also provide a value for the deprecated fish 2.0 $_ variable
    parser
        .vars()
        .set_one(L!("_"), EnvMode::GLOBAL, L!("fish").to_owned());

    initialize_tty_metadata();
}

/// Destroy data for interactive use.
fn reader_interactive_destroy() {
    Outputter::stdoutput().borrow_mut().reset_text_face();
}

/// Return whether fish is currently unwinding the stack in preparation to exit.
pub fn fish_is_unwinding_for_exit() -> bool {
    let exit_state = EXIT_STATE.load(Ordering::Relaxed);
    let exit_state: ExitState = unsafe { std::mem::transmute(exit_state) };
    match exit_state {
        ExitState::None => {
            // Cancel if we got SIGHUP.
            reader_received_sighup()
        }
        ExitState::RunningHandlers => {
            // We intend to exit but we want to allow these handlers to run.
            false
        }
        ExitState::FinishedHandlers => {
            // Done running exit handlers, time to exit.
            true
        }
    }
}

/// Write the title to the titlebar. This function is called just before a new application starts
/// executing and just after it finishes.
///
/// \param cmd Command line string passed to \c fish_title if is defined.
/// \param parser The parser to use for autoloading fish_title.
/// \param reset_cursor_position If set, issue a \r so the line driver knows where we are
pub fn reader_write_title(
    cmd: &wstr,
    parser: &Parser,
    reset_cursor_position: bool, /* = true */
) {
    let _scoped = parser.push_scope(|s| {
        s.is_interactive = false;
        s.suppress_fish_trace = true;
    });

    let mut fish_title_command = DEFAULT_TITLE.to_owned();
    if function::exists(L!("fish_title"), parser) {
        fish_title_command = L!("fish_title").to_owned();
        if !cmd.is_empty() {
            fish_title_command.push(' ');
            fish_title_command.push_utfstr(&escape_string(
                cmd,
                EscapeStringStyle::Script(EscapeFlags::NO_QUOTED | EscapeFlags::NO_TILDE),
            ));
        }
    }

    let mut lst = vec![];
    let _ = exec_subshell(
        &fish_title_command,
        parser,
        Some(&mut lst),
        /*apply_exit_status=*/ false,
    );

    let mut out = BufferedOutputter::new(Outputter::stdoutput());
    if !lst.is_empty() {
        out.write_command(Osc0WindowTitle(&lst));
    }

    out.reset_text_face();
    if reset_cursor_position && !lst.is_empty() {
        // Put the cursor back at the beginning of the line (issue #2453).
        out.write_bytes(b"\r");
    }
}

impl<'a> Reader<'a> {
    fn exec_prompt_cmd(&self, prompt_cmd: &wstr, final_prompt: bool) -> Vec<WString> {
        let mut output = vec![];
        let prompt_cmd = if final_prompt && function::exists(prompt_cmd, self.parser) {
            Cow::Owned(prompt_cmd.to_owned() + L!(" --final-rendering"))
        } else {
            Cow::Borrowed(prompt_cmd)
        };
        let _ = exec_subshell(&prompt_cmd, self.parser, Some(&mut output), false);
        output
    }

    /// Execute prompt commands based on the provided arguments. The output is inserted into prompt_buff.
    fn exec_prompt(&mut self, full_prompt: bool, final_prompt: bool) {
        // Suppress fish_trace while in the prompt.
        let _suppress_trace = self.parser.push_scope(|s| s.suppress_fish_trace = true);

        // Prompts must be run non-interactively.
        let _noninteractive = self.parser.push_scope(|s| s.is_interactive = false);

        // Suppress TTY protocols in a scoped way so that e.g. control-C can cancel the prompt.
        let mut scoped_tty = TtyHandoff::new();
        scoped_tty.disable_tty_protocols();

        // Update the termsize now.
        // This allows prompts to react to $COLUMNS.
        self.update_termsize();

        self.mode_prompt_buff.clear();
        if function::exists(MODE_PROMPT_FUNCTION_NAME, self.parser) {
            // We do not support multiline mode indicators, so just concatenate all of them.
            self.mode_prompt_buff =
                WString::from_iter(self.exec_prompt_cmd(MODE_PROMPT_FUNCTION_NAME, final_prompt));
        }

        if full_prompt {
            self.left_prompt_buff.clear();
            self.right_prompt_buff.clear();

            if !self.conf.left_prompt_cmd.is_empty() {
                // Historic compatibility hack.
                // If the left prompt function is deleted, then use a default prompt instead of
                // producing an error.
                let prompt_cmd = if self.conf.left_prompt_cmd != LEFT_PROMPT_FUNCTION_NAME
                    || function::exists(&self.conf.left_prompt_cmd, self.parser)
                {
                    &self.conf.left_prompt_cmd
                } else {
                    DEFAULT_PROMPT
                };

                self.left_prompt_buff =
                    join_strings(&self.exec_prompt_cmd(prompt_cmd, final_prompt), '\n');

                if final_prompt {
                    self.screen.multiline_prompt_hack();
                }
            }

            // Don't execute the right prompt if it is undefined fish_right_prompt
            if !self.conf.right_prompt_cmd.is_empty()
                && (self.conf.right_prompt_cmd != RIGHT_PROMPT_FUNCTION_NAME
                    || function::exists(&self.conf.right_prompt_cmd, self.parser))
            {
                // Right prompt does not support multiple lines, so just concatenate all of them.
                self.right_prompt_buff = WString::from_iter(
                    self.exec_prompt_cmd(&self.conf.right_prompt_cmd, final_prompt),
                );
            }
        }

        // Write the screen title. Do not reset the cursor position: exec_prompt is called when there
        // may still be output on the line from the previous command (#2499) and we need our PROMPT_SP
        // hack to work.
        reader_write_title(L!(""), self.parser, false);

        // Reap jobs but do NOT trigger a repaint.
        // This is to prevent infinite loops in case a job from the prompt triggers a repaint.
        // See #9796.
        job_reap(self.parser, true);

        // Some prompt may have requested an exit (#8033).
        let exit_current_script = self.parser.libdata().exit_current_script;
        self.exit_loop_requested |= exit_current_script;
        self.parser.libdata_mut().exit_current_script = false;
    }
}

#[derive(Default)]
struct Autosuggestion {
    // The text to use, as an extension/replacement of the current line.
    text: WString,

    // The range within the commandline that was searched. Always a whole line.
    search_string_range: Range<usize>,

    // Whether the autosuggestion should be case insensitive.
    // This is true for file-generated autosuggestions, but not for history.
    icase: bool,

    // Whether the autosuggestion is a whole match from history.
    is_whole_item_from_history: bool,
}

impl Autosuggestion {
    // Clear our contents.
    fn clear(&mut self) {
        self.text.clear();
    }

    // Return whether we have empty text.
    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The result of an autosuggestion computation.
#[derive(Default)]
struct AutosuggestionResult {
    // The autosuggestion.
    autosuggestion: Autosuggestion,

    // The commandline this result is based off.
    command_line: WString,

    // The list of completions which may need loading.
    needs_load: Vec<WString>,
}

impl std::ops::Deref for AutosuggestionResult {
    type Target = Autosuggestion;
    fn deref(&self) -> &Self::Target {
        &self.autosuggestion
    }
}

impl AutosuggestionResult {
    fn new(
        command_line: WString,
        search_string_range: Range<usize>,
        text: WString,
        icase: bool,
        is_whole_item_from_history: bool,
    ) -> Self {
        Self {
            autosuggestion: Autosuggestion {
                text,
                search_string_range,
                icase,
                is_whole_item_from_history,
            },
            command_line,
            needs_load: vec![],
        }
    }

    /// The line which was searched for.
    fn search_string(&self) -> &wstr {
        &self.command_line[self.search_string_range.clone()]
    }
}

// Returns a function that can be invoked (potentially
// on a background thread) to determine the autosuggestion
fn get_autosuggestion_performer(
    parser: &Parser,
    command_line: WString,
    cursor_pos: usize,
    history: Arc<History>,
) -> impl FnOnce() -> AutosuggestionResult {
    let generation_count = read_generation_count();
    let vars = parser.vars().snapshot();
    let working_directory = parser.vars().get_pwd_slash();
    move || {
        assert_is_background_thread();
        let nothing = AutosuggestionResult::default();
        let ctx = get_bg_context(&vars, generation_count);
        if ctx.check_cancel() {
            return nothing;
        }

        // Let's make sure we aren't using the empty string.
        let search_string_range = range_of_line_at_cursor(&command_line, cursor_pos);
        let search_string = &command_line[search_string_range.clone()];
        let Some(last_char) = search_string.chars().next_back() else {
            return nothing;
        };

        // Search history for a matching item unless this line is not a continuation line or quoted.
        let cursor_line_has_process_start = {
            let mut tokens = vec![];
            parse_util_process_extent(&command_line, cursor_pos, Some(&mut tokens));
            range_of_line_at_cursor(
                &command_line,
                tokens.first().map(|tok| tok.offset()).unwrap_or(cursor_pos),
            ) == search_string_range
        };
        if cursor_line_has_process_start {
            let mut searcher = HistorySearch::new_with_type(
                history,
                search_string.to_owned(),
                SearchType::LinePrefix,
            );
            while !ctx.check_cancel() && searcher.go_to_next_match(SearchDirection::Backward) {
                let item = searcher.current_item();

                // Suggest only a single line each time.
                let matched_line = item
                    .str()
                    .as_char_slice()
                    .split(|&c| c == '\n')
                    .rev()
                    .find(|line| line.starts_with(search_string.as_char_slice()))
                    .unwrap();

                if autosuggest_validate_from_history(item, &working_directory, &ctx) {
                    // The command autosuggestion was handled specially, so we're done.
                    // History items are case-sensitive, see #3978.
                    let is_whole = matched_line.len() == item.str().len();
                    return AutosuggestionResult::new(
                        command_line,
                        search_string_range,
                        matched_line.into(),
                        /*icase=*/ false,
                        is_whole,
                    );
                }
            }
        }

        // Maybe cancel here.
        if ctx.check_cancel() {
            return nothing;
        }

        // Here we do something a little funny. If the line ends with a space, and the cursor is not
        // at the end, don't use completion autosuggestions. It ends up being pretty weird seeing
        // stuff get spammed on the right while you go back to edit a line
        let cursor_at_end =
            cursor_pos == command_line.len() || command_line.as_char_slice()[cursor_pos] == '\n';
        if !cursor_at_end && last_char.is_whitespace() {
            return nothing;
        }

        // On the other hand, if the line ends with a quote, don't go dumping stuff after the quote.
        if matches!(last_char, '\'' | '"') && cursor_at_end {
            return nothing;
        }

        // Try normal completions.
        let complete_flags = CompletionRequestOptions::autosuggest();
        let mut would_be_cursor = search_string_range.end;
        let (mut completions, needs_load) =
            complete(&command_line[..would_be_cursor], complete_flags, &ctx);

        let suggestion = if completions.is_empty() {
            WString::new()
        } else {
            sort_and_prioritize(&mut completions, complete_flags);
            let comp = &completions[0];
            let full_line = completion_apply_to_command_line(
                &OperationContext::background_interruptible(&vars),
                &comp.completion,
                comp.flags,
                &command_line,
                &mut would_be_cursor,
                /*append_only=*/ true,
                /*is_unique=*/ false,
            );
            line_at_cursor(&full_line, would_be_cursor).to_owned()
        };
        let mut result = AutosuggestionResult::new(
            command_line,
            search_string_range,
            suggestion,
            true, // normal completions are case-insensitive
            /*is_whole_item_from_history=*/ false,
        );
        result.needs_load = needs_load;
        result
    }
}

enum AutosuggestionPortion {
    Count(usize),
    PerMoveWordStyle(MoveWordStyle),
}

impl<'a> Reader<'a> {
    fn can_autosuggest(&self) -> bool {
        // We autosuggest if suppress_autosuggestion is not set, if we're not doing a history search,
        // and our command line contains a non-whitespace character.
        let (elt, el) = self.active_edit_line();
        self.conf.autosuggest_ok
            && !self.suppress_autosuggestion
            && self.history_search.is_at_present()
            && elt == EditableLineTag::Commandline
            && el
                .text()
                .chars()
                .any(|c| !matches!(c, ' ' | '\t' | '\r' | '\n' | '\x0B'))
    }

    // Called after an autosuggestion has been computed on a background thread.
    fn autosuggest_completed(&mut self, result: AutosuggestionResult) {
        assert_is_main_thread();
        if result.command_line == self.data.in_flight_autosuggest_request {
            self.data.in_flight_autosuggest_request.clear();
        }
        if result.command_line != self.command_line.text() {
            // This autosuggestion is stale.
            return;
        }
        // Maybe load completions for commands discovered by this autosuggestion.
        let mut loaded_new = false;
        for to_load in &result.needs_load {
            if complete_load(to_load, self.parser) {
                FLOGF!(
                    complete,
                    "Autosuggest found new completions for %ls, restarting",
                    to_load
                );
                loaded_new = true;
            }
        }
        if loaded_new {
            // We loaded new completions for this command.
            // Re-do our autosuggestion.
            self.update_autosuggestion();
        } else if !result.is_empty()
            && self.can_autosuggest()
            && string_prefixes_string_maybe_case_insensitive(
                result.icase,
                result.search_string(),
                &result.text,
            )
        {
            // Autosuggestion is active and the search term has not changed, so we're good to go.
            self.autosuggestion = result.autosuggestion;
            if self.is_repaint_needed(None) {
                self.layout_and_repaint(L!("autosuggest"));
            }
        }
    }

    fn update_autosuggestion(&mut self) {
        // If we can't autosuggest, just clear it.
        if !self.can_autosuggest() {
            self.data.in_flight_autosuggest_request.clear();
            self.data.autosuggestion.clear();
            return;
        }

        let el = &self.data.command_line;
        let autosuggestion = &self.autosuggestion;
        if self.is_at_line_with_autosuggestion() {
            assert!(string_prefixes_string_maybe_case_insensitive(
                autosuggestion.icase,
                &el.text()[autosuggestion.search_string_range.clone()],
                &autosuggestion.text
            ));
            return;
        }

        // Do nothing if we've already kicked off this autosuggest request.
        if el.text() == self.in_flight_autosuggest_request {
            return;
        }
        self.data.in_flight_autosuggest_request = el.text().to_owned();

        // Clear the autosuggestion and kick it off in the background.
        FLOG!(reader_render, "Autosuggesting");
        self.data.autosuggestion.clear();
        let performer = get_autosuggestion_performer(
            self.parser,
            el.text().to_owned(),
            el.position(),
            self.history.clone(),
        );
        let canary = Rc::downgrade(&self.canary);
        let completion = move |zelf: &mut Reader, result| {
            if canary.upgrade().is_none() {
                return;
            }
            zelf.autosuggest_completed(result);
        };
        debounce_autosuggestions().perform_with_completion(performer, completion);
    }

    fn is_at_end(&self) -> bool {
        let (_elt, el) = self.active_edit_line();
        match self.cursor_end_mode {
            CursorEndMode::Exclusive => el.position() == el.len(),
            CursorEndMode::Inclusive => el.position() + 1 >= el.len(),
        }
    }

    fn is_at_autosuggestion(&self) -> bool {
        if self.active_edit_line_tag() != EditableLineTag::Commandline {
            return false;
        }
        let autosuggestion = &self.autosuggestion;
        if autosuggestion.is_empty() {
            return false;
        }
        let el = &self.command_line;
        (match self.cursor_end_mode {
            CursorEndMode::Exclusive => el.position(),
            CursorEndMode::Inclusive => el.position() + 1,
        }) == autosuggestion.search_string_range.end
    }

    fn is_at_line_with_autosuggestion(&self) -> bool {
        if self.active_edit_line_tag() != EditableLineTag::Commandline {
            return false;
        }
        let autosuggestion = &self.autosuggestion;
        if autosuggestion.is_empty() {
            return false;
        }
        let el = &self.command_line;
        range_of_line_at_cursor(el.text(), el.position()) == autosuggestion.search_string_range
    }

    // Accept any autosuggestion by replacing the command line with it. If full is true, take the whole
    // thing; if it's false, then respect the passed in style.
    fn accept_autosuggestion(&mut self, amount: AutosuggestionPortion) {
        assert!(self.is_at_line_with_autosuggestion());

        // Accepting an autosuggestion clears the pager.
        self.clear_pager();

        let autosuggestion = &self.autosuggestion;
        let autosuggestion_text = &autosuggestion.text;
        let search_string_range = autosuggestion.search_string_range.clone();
        // Accept the autosuggestion.
        let (range, replacement) = match amount {
            AutosuggestionPortion::Count(count) => {
                if count == usize::MAX {
                    (search_string_range, autosuggestion_text.clone())
                } else {
                    let pos = search_string_range.end;
                    let available = autosuggestion_text.len() - search_string_range.len();
                    let count = count.min(available);
                    if count == 0 {
                        return;
                    }
                    let start = autosuggestion_text.len() - available;
                    (
                        pos..pos,
                        autosuggestion_text[start..start + count].to_owned(),
                    )
                }
            }
            AutosuggestionPortion::PerMoveWordStyle(style) => {
                // Accept characters according to the specified style.
                let mut state = MoveWordStateMachine::new(style);
                let have = search_string_range.len();
                let mut want = have;
                while want < autosuggestion_text.len() {
                    let wc = autosuggestion_text.as_char_slice()[want];
                    if !state.consume_char(wc) {
                        break;
                    }
                    want += 1;
                }
                (
                    search_string_range.end..search_string_range.end,
                    autosuggestion_text[have..want].to_owned(),
                )
            }
        };
        self.data
            .replace_substring(EditableLineTag::Commandline, range, replacement);
    }
}

#[derive(Default)]
struct HighlightResult {
    colors: Vec<HighlightSpec>,
    text: WString,
}

// Given text and  whether IO is allowed, return a function that performs highlighting. The function
// may be invoked on a background thread.
fn get_highlight_performer(
    parser: &Parser,
    el: &EditableLine,
    io_ok: bool,
) -> impl FnOnce() -> HighlightResult {
    let vars = parser.vars().snapshot();
    let generation_count = read_generation_count();
    let position = el.position();
    let text = el.text().to_owned();
    move || {
        if text.is_empty() {
            return HighlightResult::default();
        }
        let ctx = get_bg_context(&vars, generation_count);
        let mut colors = vec![];
        highlight_shell(&text, &mut colors, &ctx, io_ok, Some(position));
        HighlightResult { colors, text }
    }
}

impl<'a> Reader<'a> {
    fn highlight_complete(&mut self, result: HighlightResult) {
        assert_is_main_thread();
        self.in_flight_highlight_request.clear();
        if result.text == self.command_line.text() {
            assert_eq!(result.colors.len(), self.command_line.len());
            if self.is_repaint_needed(Some(&result.colors)) {
                self.command_line.set_colors(result.colors);
                self.layout_and_repaint(L!("highlight"));
            }
        }
    }

    /// Highlight the command line in a super, plentiful way.
    fn super_highlight_me_plenty(&mut self) {
        if !self.conf.highlight_ok {
            return;
        }

        // Do nothing if this text is already in flight.
        if self.command_line.text() == self.in_flight_highlight_request {
            return;
        }
        self.in_flight_highlight_request = self.command_line.text().to_owned();

        FLOG!(reader_render, "Highlighting");
        let highlight_performer =
            get_highlight_performer(self.parser, &self.command_line, /*io_ok=*/ true);
        let canary = Rc::downgrade(&self.canary);
        let completion = move |zelf: &mut Reader, result| {
            if canary.upgrade().is_none() {
                return;
            }
            zelf.highlight_complete(result);
        };
        debounce_highlighting().perform_with_completion(highlight_performer, completion);
    }

    /// Finish up any outstanding syntax highlighting, before execution.
    /// This plays some tricks to not block on I/O for too long.
    fn finish_highlighting_before_exec(&mut self) {
        // Early-out if highlighting is not OK.
        if !self.conf.highlight_ok {
            return;
        }

        // Decide if our current highlighting is OK. If not we will do a quick highlight without I/O.
        let mut current_highlight_ok = false;
        if self.in_flight_highlight_request.is_empty() {
            // There is no in-flight highlight request. Two possibilities:
            // 1: The user hit return after highlighting finished, so current highlighting is correct.
            // 2: The user hit return before highlighting started, so current highlighting is stale.
            // We can distinguish these based on what we last rendered.
            current_highlight_ok = self.rendered_layout.text == self.command_line.text();
        } else if self.in_flight_highlight_request == self.command_line.text() {
            // The user hit return while our in-flight highlight request was still processing the text.
            // Wait for its completion to run, but not forever.
            let mut now = Instant::now();
            let deadline = now + HIGHLIGHT_TIMEOUT_FOR_EXECUTION;
            while now < deadline {
                let timeout = deadline - now;
                iothread_service_main_with_timeout(self, timeout);

                // Note iothread_service_main_with_timeout will reentrantly modify us,
                // by invoking a completion.
                if self.in_flight_highlight_request.is_empty() {
                    break;
                }
                now = Instant::now();
            }

            // If our in_flight_highlight_request is now empty, it means it completed and we highlighted
            // successfully.
            current_highlight_ok = self.in_flight_highlight_request.is_empty();
        }

        if !current_highlight_ok {
            // We need to do a quick highlight without I/O.
            let highlight_no_io =
                get_highlight_performer(self.parser, &self.command_line, /*io_ok=*/ false);
            self.highlight_complete(highlight_no_io());
        }
    }
}

struct HistoryPagerResult {
    matched_commands: Vec<Completion>,
    range: Range<usize>,
    first_shown: usize,
    motion: Option<SelectionMotion>,
}

#[derive(Eq, PartialEq)]
enum HistoryPagerInvocation {
    Anew,
    Advance,
    Refresh,
}

fn history_pager_search(
    history: &Arc<History>,
    direction: SearchDirection,
    motion: Option<SelectionMotion>,
    history_index: usize,
    search_string: &wstr,
) -> HistoryPagerResult {
    // Limit the number of elements to half the screen like we do for completions
    // Note that this is imperfect because we could have a multi-column layout.
    //
    // We can still push fish further upward in case the first entry is multiline,
    // but that can't really be helped.
    // (subtract 2 for the search line and the prompt)
    let page_size = usize::try_from(cmp::max(termsize_last().height / 2 - 2, 12)).unwrap();
    let mut completions = Vec::with_capacity(page_size);
    let mut search = HistorySearch::new_with(
        history.clone(),
        search_string.to_owned(),
        SearchType::ContainsGlob,
        smartcase_flags(search_string),
        history_index,
    );
    if !search.go_to_next_match(direction) && !parse_util_contains_wildcards(search_string) {
        // If there were no matches, and the user is not intending for
        // wildcard search, try again with subsequence search.
        search = HistorySearch::new_with(
            history.clone(),
            search_string.to_owned(),
            SearchType::ContainsSubsequence,
            smartcase_flags(search_string),
            history_index,
        );
        search.go_to_next_match(direction);
    }
    // When searching, first we need to find the element before first shown.
    search.search_forward(match direction {
        SearchDirection::Forward => page_size,
        SearchDirection::Backward => 0,
    });
    let first_index = search.current_index();
    let mut next_match_found = search.go_to_next_match(SearchDirection::Backward);
    let first_shown = search.current_index();
    while completions.len() < page_size && next_match_found {
        let item = search.current_item();
        completions.push(Completion::new(
            item.str().to_owned(),
            L!("").to_owned(),
            StringFuzzyMatch::exact_match(),
            CompleteFlags::REPLACES_LINE | CompleteFlags::DONT_ESCAPE | CompleteFlags::DONT_SORT,
        ));
        next_match_found = search.go_to_next_match(SearchDirection::Backward);
    }
    let last_index = search.current_index();
    let range = first_index..last_index;
    if completions.is_empty() && range != (0..history.size() + 1) {
        history_pager_search(
            history,
            SearchDirection::Forward,
            Some(SelectionMotion::Prev),
            history.size() + 1,
            search_string,
        )
    } else {
        HistoryPagerResult {
            matched_commands: completions,
            range,
            first_shown,
            motion,
        }
    }
}

impl ReaderData {
    fn fill_history_pager(
        &mut self,
        why: HistoryPagerInvocation,
        motion: Option<SelectionMotion>,
        mut direction: SearchDirection, /* = Backward */
    ) {
        let index;
        let mut old_pager_index = None;
        match why {
            HistoryPagerInvocation::Anew => {
                assert_eq!(direction, SearchDirection::Backward);
                index = 0;
            }
            HistoryPagerInvocation::Advance => {
                let history_pager = self.history_pager.as_ref().unwrap();
                index = match direction {
                    SearchDirection::Forward => history_pager.start + 1,
                    SearchDirection::Backward => history_pager.end - 1,
                }
            }
            HistoryPagerInvocation::Refresh => {
                // Make backward search from current position
                let history_pager = self.history_pager.as_ref().unwrap();
                direction = SearchDirection::Backward;
                index = history_pager.start;
                old_pager_index = self.pager.selected_completion_index();
            }
        }
        let search_term = self.pager.search_field_line.text().to_owned();
        let performer = {
            let history = self.history.clone();
            let search_term = search_term.clone();
            move || history_pager_search(&history, direction, motion, index, &search_term)
        };
        let canary = Rc::downgrade(&self.canary);
        let completion = move |zelf: &mut Reader, result: HistoryPagerResult| {
            if canary.upgrade().is_none() {
                return;
            }
            if search_term != zelf.pager.search_field_line.text() {
                return; // Stale request.
            }
            let history_size = zelf.history.size();
            let Some(history_pager) = zelf.history_pager.as_mut() else {
                return; // Pager has been closed.
            };
            assert!(result.range.start < result.range.end);
            *history_pager = result.range;
            zelf.pager.extra_progress_text =
                if !result.matched_commands.is_empty() && *history_pager != (0..history_size + 1) {
                    wgettext_fmt!(
                        "Items %lu to %lu of %lu",
                        match history_pager.start {
                            0 => 1,
                            _ => result.first_shown,
                        },
                        history_pager.end - 1,
                        history_size
                    )
                } else {
                    L!("").to_owned()
                };
            zelf.pager.set_completions(&result.matched_commands, false);
            if why == HistoryPagerInvocation::Refresh {
                zelf.pager.set_selected_completion_index(old_pager_index);
                zelf.pager_selection_changed();
            }
            if let Some(motion) = result.motion {
                zelf.select_completion_in_direction(motion, true);
            }
            zelf.super_highlight_me_plenty();
            zelf.layout_and_repaint(L!("history-pager"));
        };
        let debouncer = debounce_history_pager();
        debouncer.perform_with_completion(performer, completion);
    }
}

/// Expand an abbreviation replacer, which may mean running its function.
/// Return the replacement, or none to skip it. This may run fish script!
fn expand_replacer(
    range: SourceRange,
    token: &wstr,
    repl: &abbrs::Replacer,
    parser: &Parser,
) -> Option<abbrs::Replacement> {
    if !repl.is_function {
        // Literal replacement cannot fail.
        FLOGF!(
            abbrs,
            "Expanded literal abbreviation <%ls> -> <%ls>",
            token,
            &repl.replacement
        );
        return Some(abbrs::Replacement::new(
            range,
            repl.replacement.clone(),
            repl.set_cursor_marker.clone(),
        ));
    }

    let mut cmd = escape(&repl.replacement);
    cmd.push(' ');
    cmd.push_utfstr(&escape(token));
    let _not_interactive = parser.push_scope(|s| {
        s.is_interactive = false;
        s.readonly_commandline = true;
    });

    let mut outputs = vec![];
    if exec_subshell(
        &cmd,
        parser,
        Some(&mut outputs),
        /*apply_exit_status=*/ false,
    )
    .is_err()
    {
        return None;
    }
    let result = join_strings(&outputs, '\n');
    FLOGF!(
        abbrs,
        "Expanded function abbreviation <%ls> -> <%ls>",
        token,
        result
    );

    Some(abbrs::Replacement::new(
        range,
        result,
        repl.set_cursor_marker.clone(),
    ))
}

// Extract all the token ranges in `str`, along with whether they are a command.
// Tokens containing command substitutions are skipped; this ensures tokens are non-overlapping.
struct PositionedToken {
    range: SourceRange,
    is_cmd: bool,
}

fn extract_tokens(s: &wstr) -> Vec<PositionedToken> {
    let ast_flags = ParseTreeFlags::CONTINUE_AFTER_ERROR
        | ParseTreeFlags::ACCEPT_INCOMPLETE_TOKENS
        | ParseTreeFlags::LEAVE_UNTERMINATED;
    let ast = ast::parse(s, ast_flags, None);

    let mut result = vec![];
    let mut traversal = ast.walk();
    while let Some(node) = traversal.next() {
        // We are only interested in leaf nodes with source.
        if node.as_leaf().is_none() {
            continue;
        };
        let range = node.source_range();
        if range.length() == 0 {
            continue;
        }

        // If we have command subs, then we don't include this token; instead we recurse.
        let mut has_cmd_subs = false;
        let mut cmdsub_cursor = range.start();
        loop {
            match parse_util_locate_cmdsubst_range(
                s,
                &mut cmdsub_cursor,
                /*accept_incomplete=*/ true,
                None,
                None,
            ) {
                MaybeParentheses::Error | MaybeParentheses::None => break,
                MaybeParentheses::CommandSubstitution(parens) => {
                    if parens.start() >= range.end() {
                        break;
                    }
                    has_cmd_subs = true;
                    for mut t in extract_tokens(&s[parens.command()]) {
                        t.range.start += u32::try_from(parens.command().start).unwrap();
                        result.push(t);
                    }
                }
            }
        }

        if !has_cmd_subs {
            // Common case of no command substitutions in this leaf node.
            // Check if a node is the command portion of a decorated statement.
            let mut is_cmd = false;
            if let Kind::DecoratedStatement(stmt) = traversal.parent(node).kind() {
                is_cmd = is_same_node(node, &stmt.command);
            }
            result.push(PositionedToken { range, is_cmd })
        }
    }

    result
}

/// Expand at most one abbreviation at the given cursor position, updating the position if the
/// abbreviation wants to move the cursor. Use the parser to run any abbreviations which want
/// function calls. Return none if no abbreviations were expanded, otherwise the resulting
/// replacement.
pub fn reader_expand_abbreviation_at_cursor(
    cmdline: &wstr,
    cursor_pos: usize,
    parser: &Parser,
) -> Option<abbrs::Replacement> {
    // Find the token containing the cursor. Usually users edit from the end, so walk backwards.
    let tokens = extract_tokens(cmdline);
    let mut token: Option<_> = None;
    let mut cmdtok: Option<_> = None;

    for t in tokens.into_iter().rev() {
        let range = t.range;
        let is_cmd = t.is_cmd;
        if t.range.contains_inclusive(cursor_pos) {
            token = Some(t);
        }
        // The command is at or *before* the token the cursor is on,
        // and once we have a command we can stop.
        if token.is_some() && is_cmd {
            cmdtok = Some(range);
            break;
        }
    }
    let token = token?;
    let range = token.range;
    let position = if token.is_cmd {
        abbrs::Position::Command
    } else {
        abbrs::Position::Anywhere
    };
    // If the token itself is the command, we have no command to pass.
    let cmd = if !token.is_cmd {
        cmdtok.map(|t| &cmdline[Range::<usize>::from(t)])
    } else {
        None
    };

    let token_str = &cmdline[Range::<usize>::from(range)];
    let replacers = abbrs_match(token_str, position, cmd.unwrap_or(L!("")));
    for replacer in replacers {
        if let Some(replacement) = expand_replacer(range, token_str, &replacer, parser) {
            return Some(replacement);
        }
    }
    None
}

impl<'a> Reader<'a> {
    /// Expand abbreviations at the current cursor position, minus the given cursor backtrack. This
    /// may change the command line but does NOT repaint it. This is to allow the caller to coalesce
    /// repaints.
    fn expand_abbreviation_at_cursor(&mut self, cursor_backtrack: usize) -> bool {
        let (elt, el) = self.active_edit_line();
        if self.conf.expand_abbrev_ok && elt == EditableLineTag::Commandline {
            // Try expanding abbreviations.
            let cursor_pos = el.position().saturating_sub(cursor_backtrack);
            if let Some(replacement) =
                reader_expand_abbreviation_at_cursor(el.text(), cursor_pos, self.parser)
            {
                self.push_edit(elt, Edit::new(replacement.range.into(), replacement.text));
                self.update_buff_pos(elt, replacement.cursor);
                return true;
            }
        }
        false
    }
}

/// Indicates if the given command char ends paging.
fn command_ends_paging(c: ReadlineCmd, focused_on_search_field: bool) -> bool {
    #[allow(non_camel_case_types)]
    type rl = ReadlineCmd;
    match c {
        rl::HistoryPrefixSearchBackward
        | rl::HistoryPrefixSearchForward
        | rl::HistorySearchBackward
        | rl::HistorySearchForward
        | rl::HistoryTokenSearchBackward
        | rl::HistoryTokenSearchForward
        | rl::HistoryLastTokenSearchBackward
        | rl::HistoryLastTokenSearchForward
        | rl::AcceptAutosuggestion
        | rl::DeleteOrExit
        | rl::CancelCommandline
        | rl::ClearCommandline
        | rl::Cancel =>
        // These commands always end paging.
        {
            true
        }
        rl::Complete
        | rl::CompleteAndSearch
        | rl::HistoryPager
        | rl::BackwardChar
        | rl::BackwardCharPassive
        | rl::ForwardChar
        | rl::ForwardCharPassive
        | rl::ForwardSingleChar
        | rl::UpLine
        | rl::DownLine
        | rl::Repaint
        | rl::SuppressAutosuggestion
        | rl::BeginningOfHistory
        | rl::EndOfHistory =>
        // These commands never end paging.
        {
            false
        }
        rl::Execute =>
        // execute does end paging, but only executes if it was not paging. So it's handled
        // specially.
        {
            false
        }
        rl::BeginningOfLine
        | rl::EndOfLine
        | rl::ForwardWord
        | rl::BackwardWord
        | rl::ForwardBigword
        | rl::BackwardBigword
        | rl::ForwardToken
        | rl::BackwardToken
        | rl::NextdOrForwardWord
        | rl::PrevdOrBackwardWord
        | rl::DeleteChar
        | rl::BackwardDeleteChar
        | rl::KillLine
        | rl::Yank
        | rl::YankPop
        | rl::BackwardKillLine
        | rl::KillWholeLine
        | rl::KillInnerLine
        | rl::KillWord
        | rl::KillBigword
        | rl::KillToken
        | rl::BackwardKillWord
        | rl::BackwardKillPathComponent
        | rl::BackwardKillBigword
        | rl::BackwardKillToken
        | rl::SelfInsert
        | rl::SelfInsertNotFirst
        | rl::TransposeChars
        | rl::TransposeWords
        | rl::UpcaseWord
        | rl::DowncaseWord
        | rl::CapitalizeWord
        | rl::BeginningOfBuffer
        | rl::EndOfBuffer
        | rl::Undo
        | rl::Redo =>
        // These commands operate on the search field if that's where the focus is.
        {
            !focused_on_search_field
        }
        _ => false,
    }
}

/// Indicates if the given command ends the history search.
fn command_ends_history_search(c: ReadlineCmd) -> bool {
    #[allow(non_camel_case_types)]
    type rl = ReadlineCmd;
    #[allow(deprecated)]
    !matches!(
        c,
        rl::HistoryPrefixSearchBackward
            | rl::HistoryPrefixSearchForward
            | rl::HistorySearchBackward
            | rl::HistorySearchForward
            | rl::HistoryTokenSearchBackward
            | rl::HistoryTokenSearchForward
            | rl::HistoryLastTokenSearchBackward
            | rl::HistoryLastTokenSearchForward
            | rl::HistoryDelete
            | rl::HistoryPagerDelete
            | rl::BeginningOfHistory
            | rl::EndOfHistory
            | rl::Repaint
            | rl::ForceRepaint
    )
}

/// Return true if we believe ourselves to be orphaned. loop_count is how many times we've tried to
/// stop ourselves via SIGGTIN.
fn check_for_orphaned_process(loop_count: usize, shell_pgid: libc::pid_t) -> bool {
    let mut we_think_we_are_orphaned = false;
    // Try kill-0'ing the process whose pid corresponds to our process group ID. It's possible this
    // will fail because we don't have permission to signal it. But more likely it will fail because
    // it no longer exists, and we are orphaned.
    if loop_count % 64 == 0 && unsafe { libc::kill(shell_pgid, 0) } < 0 && errno().0 == ESRCH {
        we_think_we_are_orphaned = true;
    }

    // Try reading from the tty; if we get EIO we are orphaned. This is sort of bad because it
    // may block.
    if !we_think_we_are_orphaned && loop_count % 128 == 0 {
        extern "C" {
            fn ctermid(buf: *mut c_char) -> *mut c_char;
        }
        let tty = unsafe { ctermid(std::ptr::null_mut()) };
        if tty.is_null() {
            perror("ctermid");
            exit_without_destructors(1);
        }

        // Open the tty. Presumably this is stdin, but maybe not?
        let tty_fd = AutoCloseFd::new(unsafe { libc::open(tty, O_RDONLY | O_NONBLOCK) });
        if !tty_fd.is_valid() {
            perror("open");
            exit_without_destructors(1);
        }

        let mut tmp = 0 as libc::c_char;
        if unsafe {
            libc::read(
                tty_fd.fd(),
                &mut tmp as *mut libc::c_char as *mut libc::c_void,
                1,
            )
        } < 0
            && errno().0 == EIO
        {
            we_think_we_are_orphaned = true;
        }
    }

    // Just give up if we've done it a lot times.
    if loop_count > 4096 {
        we_think_we_are_orphaned = true;
    }

    we_think_we_are_orphaned
}

/// Run the specified command with the correct terminal modes, and while taking care to perform job
/// notification, set the title, etc.
fn reader_run_command(parser: &Parser, cmd: &wstr) -> EvalRes {
    assert!(
        !get_tty_protocols_active(),
        "TTY protocols should not be active"
    );
    let ft = tok_command(cmd);

    // Provide values for `status current-command` and `status current-commandline`
    if !ft.is_empty() {
        parser.libdata_mut().status_vars.command = ft.to_owned();
        parser.libdata_mut().status_vars.commandline = cmd.to_owned();
        // Also provide a value for the deprecated fish 2.0 $_ variable
        parser
            .vars()
            .set_one(L!("_"), EnvMode::GLOBAL, ft.to_owned());
    }

    reader_write_title(cmd, parser, true);
    Outputter::stdoutput()
        .borrow_mut()
        .set_text_face(TextFace::default());
    term_donate(false);

    let time_before = Instant::now();
    let eval_res = parser.eval(cmd, &IoChain::new());
    job_reap(parser, true);

    // Update the execution duration iff a command is requested for execution
    // issue - #4926
    if !ft.is_empty() {
        let time_after = Instant::now();
        let duration = time_after.duration_since(time_before);
        parser.vars().set_one(
            ENV_CMD_DURATION,
            EnvMode::UNEXPORT,
            duration.as_millis().to_wstring(),
        );
    }

    term_steal(eval_res.status.is_success());

    // Provide value for `status current-command`
    parser.libdata_mut().status_vars.command = (*PROGRAM_NAME.get().unwrap()).to_owned();
    // Also provide a value for the deprecated fish 2.0 $_ variable
    parser.vars().set_one(
        L!("_"),
        EnvMode::GLOBAL,
        (*PROGRAM_NAME.get().unwrap()).to_owned(),
    );
    // Provide value for `status current-commandline`
    parser.libdata_mut().status_vars.commandline = L!("").to_owned();

    if have_proc_stat() {
        proc_update_jiffies(parser);
    }

    eval_res
}

fn reader_shell_test(parser: &Parser, bstr: &wstr) -> Result<(), ParserTestErrorBits> {
    let mut errors = vec![];
    let res = parse_util_detect_errors(bstr, Some(&mut errors), /*accept_incomplete=*/ true);

    if res.is_err_and(|err| err.contains(ParserTestErrorBits::ERROR)) {
        let mut error_desc = parser.get_backtrace(bstr, &errors);

        // Ensure we end with a newline. Also add an initial newline, because it's likely the user
        // just hit enter and so there's junk on the current line.
        if !error_desc.ends_with('\n') {
            error_desc.push('\n');
        }
        eprintf!("\n%s", error_desc);
        reader_schedule_prompt_repaint();
    }
    res
}

impl<'a> Reader<'a> {
    // Import history from older location (config path) if our current history is empty.
    fn import_history_if_necessary(&mut self) {
        if self.history.is_empty() {
            self.history.populate_from_config_path();
        }

        // Import history from bash, etc. if our current history is still empty and is the default
        // history.
        if self.history.is_empty() && self.history.is_default() {
            // Try opening a bash file. We make an effort to respect $HISTFILE; this isn't very complete
            // (AFAIK it doesn't have to be exported), and to really get this right we ought to ask bash
            // itself. But this is better than nothing.
            let var = self.vars().get(L!("HISTFILE"));
            let mut path =
                var.map_or_else(|| L!("~/.bash_history").to_owned(), |var| var.as_string());
            expand_tilde(&mut path, self.vars());

            let Ok(file) = wopen_cloexec(&path, OFlag::O_RDONLY, Mode::empty()) else {
                return;
            };
            self.history.populate_from_bash(BufReader::new(file));
        }
    }

    fn should_add_to_history(&mut self, text: &wstr) -> bool {
        let parser = self.parser;
        if !function::exists(L!("fish_should_add_to_history"), parser) {
            // Historical behavior, if the command starts with a space we don't save it.
            return text.as_char_slice()[0] != ' ';
        }

        let mut cmd: WString = L!("fish_should_add_to_history ").into();
        cmd.push_utfstr(&escape(text));
        let _not_interactive = parser.push_scope(|s| s.is_interactive = false);

        exec_subshell(&cmd, parser, None, /*apply_exit_status=*/ false).is_ok()
    }

    // Add the current command line contents to history.
    fn add_to_history(&mut self) {
        if self.conf.in_silent_mode {
            return;
        }

        // Historical behavior is to trim trailing spaces, unless escape (#7661).
        let mut text = self.command_line.text().to_owned();
        while text
            .chars()
            .next_back()
            .is_some_and(|c| matches!(c, ' ' | '\n'))
            && count_preceding_backslashes(&text, text.len() - 1) % 2 == 0
        {
            text.pop();
        }

        // Remove ephemeral items - even if the text is empty.
        self.history.remove_ephemeral_items();

        if !text.is_empty() {
            // Mark this item as ephemeral if should_add_to_history says no (#615).
            let mode = if !self.should_add_to_history(&text) {
                PersistenceMode::Ephemeral
            } else if in_private_mode(self.vars()) {
                PersistenceMode::Memory
            } else {
                PersistenceMode::Disk
            };
            self.history.clone().add_pending_with_file_detection(
                &text,
                &self.parser.variables,
                mode,
            );
        }
    }

    /// Check if we have background jobs that we have not warned about.
    /// If so, print a warning and return true. Otherwise return false.
    fn try_warn_on_background_jobs(&mut self) -> bool {
        assert_is_main_thread();
        // Have we already warned?
        if self.did_warn_for_bg_jobs {
            return false;
        }
        // Are we the top-level reader?
        if reader_data_stack().len() > 1 {
            return false;
        }
        // Do we have background jobs?
        let bg_jobs = jobs_requiring_warning_on_exit(self.parser);
        if bg_jobs.is_empty() {
            return false;
        }
        // Print the warning!
        print_exit_warning_for_jobs(&bg_jobs);
        self.did_warn_for_bg_jobs = true;
        true
    }
}

/// Check if we should exit the reader loop.
/// Return true if we should exit.
pub fn check_exit_loop_maybe_warning(data: Option<&mut Reader>) -> bool {
    // sighup always forces exit.
    if reader_received_sighup() {
        return true;
    }

    // Check if an exit is requested.
    let Some(data) = data else {
        return false;
    };
    if !data.exit_loop_requested {
        return false;
    }

    if data.try_warn_on_background_jobs() {
        data.exit_loop_requested = false;
        return false;
    }
    true
}

/// Given that the user is tab-completing a token `wc` whose cursor is at `pos` in the token,
/// try expanding it as a wildcard, populating `result` with the expanded string.
fn try_expand_wildcard(
    parser: &Parser,
    wc: WString,
    position: usize,
    result: &mut WString,
) -> ExpandResultCode {
    // Hacky from #8593: only expand if there are wildcards in the "current path component."
    // Find the "current path component" by looking for an unescaped slash before and after
    // our position.
    // This is quite naive; for example it mishandles brackets.
    let is_path_sep =
        |offset| wc.char_at(offset) == '/' && count_preceding_backslashes(&wc, offset) % 2 == 0;

    let mut comp_start = position;
    while comp_start > 0 && !is_path_sep(comp_start - 1) {
        comp_start -= 1;
    }
    let mut comp_end = position;
    while comp_end < wc.len() && !is_path_sep(comp_end) {
        comp_end += 1;
    }
    if !wildcard_has(&wc[comp_start..comp_end]) {
        return ExpandResultCode::wildcard_no_match;
    }
    result.clear();
    // Have a low limit on the number of matches, otherwise we will overwhelm the command line.

    /// When tab-completing with a wildcard, we expand the wildcard up to this many results.
    /// If expansion would exceed this many results, beep and do nothing.
    const TAB_COMPLETE_WILDCARD_MAX_EXPANSION: usize = 256;

    let ctx = OperationContext::background_with_cancel_checker(
        &parser.variables,
        Box::new(|| signal_check_cancel() != 0),
        TAB_COMPLETE_WILDCARD_MAX_EXPANSION,
    );

    // We do wildcards only.

    let flags = ExpandFlags::FAIL_ON_CMDSUBST
        | ExpandFlags::SKIP_VARIABLES
        | ExpandFlags::PRESERVE_HOME_TILDES;
    let mut expanded = CompletionList::new();
    let ret = expand_string(wc, &mut expanded, flags, &ctx, None);
    if ret.result != ExpandResultCode::ok {
        return ret.result;
    }

    // Insert all matches (escaped) and a trailing space.
    let mut joined = WString::new();
    for r#match in expanded {
        if r#match.flags.contains(CompleteFlags::DONT_ESCAPE) {
            joined.push_utfstr(&r#match.completion);
        } else {
            let tildeflag = if r#match.flags.contains(CompleteFlags::DONT_ESCAPE_TILDES) {
                EscapeFlags::NO_TILDE
            } else {
                EscapeFlags::default()
            };
            joined.push_utfstr(&escape_string(
                &r#match.completion,
                EscapeStringStyle::Script(EscapeFlags::NO_QUOTED | tildeflag),
            ));
        }
        joined.push(' ');
    }

    *result = joined;
    ExpandResultCode::ok
}

/// Test if the specified character in the specified string is backslashed. pos may be at the end of
/// the string, which indicates if there is a trailing backslash.
pub(crate) fn is_backslashed(s: &wstr, pos: usize) -> bool {
    // note pos == str.size() is OK.
    if pos > s.len() {
        return false;
    }

    let mut count = 0;
    for idx in (0..pos).rev() {
        if s.as_char_slice()[idx] != '\\' {
            break;
        }
        count += 1;
    }

    count % 2 == 1
}

fn unescaped_quote(s: &wstr, pos: usize) -> Option<char> {
    let mut result = None;
    if pos < s.len() {
        let c = s.as_char_slice()[pos];
        if matches!(c, '\'' | '"') && !is_backslashed(s, pos) {
            result = Some(c);
        }
    }
    result
}

fn replace_line_at_cursor(
    text: &wstr,
    inout_cursor_pos: &mut usize,
    replacement: &wstr,
) -> WString {
    let cursor = *inout_cursor_pos;
    let start = text[0..cursor]
        .as_char_slice()
        .iter()
        .rposition(|&c| c == '\n')
        .map(|newline| newline + 1)
        .unwrap_or(0);
    let end = text[cursor..]
        .as_char_slice()
        .iter()
        .position(|&c| c == '\n')
        .map(|pos| cursor + pos)
        .unwrap_or(text.len());
    *inout_cursor_pos = start + replacement.len();
    text[..start].to_owned() + replacement + &text[end..]
}

pub(crate) fn get_quote(cmd_str: &wstr, len: usize) -> Option<char> {
    let cmd = cmd_str.as_char_slice();
    let mut i = 0;
    while i < cmd.len() {
        if cmd[i] == '\\' {
            i += 1;
            if i == cmd_str.len() {
                return None;
            }
            i += 1;
        } else if cmd[i] == '\'' || cmd[i] == '"' {
            match quote_end(cmd_str, i, cmd[i]) {
                Some(end) => {
                    if end > len {
                        return Some(cmd[i]);
                    }
                    i = end + 1;
                }
                None => return Some(cmd[i]),
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Apply a completion string. Exposed for testing only.
///
/// Insert the string in the given command line at the given cursor position. The function checks if
/// the string is quoted or not and correctly escapes the string.
///
/// \param val the string to insert
/// \param flags A union of all flags describing the completion to insert. See the completion_t
/// struct for more information on possible values.
/// \param command_line The command line into which we will insert
/// \param inout_cursor_pos On input, the location of the cursor within the command line. On output,
/// the new desired position.
/// \param append_only Whether we can only append to the command line, or also modify previous
/// characters. This is used to determine whether we go inside a trailing quote.
///
/// Return The completed string
pub fn completion_apply_to_command_line(
    ctx: &OperationContext,
    val_str: &wstr,
    flags: CompleteFlags,
    command_line: &wstr,
    inout_cursor_pos: &mut usize,
    append_only: bool,
    is_unique: bool,
) -> WString {
    let mut trailer = (!flags.contains(CompleteFlags::NO_SPACE)).then_some(' ');
    let do_replace_token = flags.contains(CompleteFlags::REPLACES_TOKEN);
    let do_replace_line = flags.contains(CompleteFlags::REPLACES_LINE);
    let do_escape = !flags.contains(CompleteFlags::DONT_ESCAPE);
    let no_tilde = flags.contains(CompleteFlags::DONT_ESCAPE_TILDES);
    let keep_variable_override = flags.contains(CompleteFlags::KEEP_VARIABLE_OVERRIDE_PREFIX);
    let is_variable_name = flags.contains(CompleteFlags::VARIABLE_NAME);

    let cursor_pos = *inout_cursor_pos;
    let mut back_into_trailing_quote = false;
    assert!(!is_variable_name || command_line.char_at(cursor_pos) != '/');
    let have_trailer = command_line.char_at(cursor_pos) == ' ';

    if do_replace_line {
        assert!(!do_escape, "unsupported completion flag");
        let cmdsub = parse_util_cmdsubst_extent(command_line, cursor_pos);
        return if !command_line[cmdsub.clone()].contains('\n') {
            *inout_cursor_pos = cmdsub.start + val_str.len();
            command_line[..cmdsub.start].to_owned() + val_str + &command_line[cmdsub.end..]
        } else {
            replace_line_at_cursor(command_line, inout_cursor_pos, val_str)
        };
    }

    let mut escape_flags = EscapeFlags::empty();
    if append_only || !is_unique || trailer.is_none() {
        escape_flags.insert(EscapeFlags::NO_QUOTED);
    }
    if no_tilde {
        escape_flags.insert(EscapeFlags::NO_TILDE);
    }

    let maybe_add_slash = |trailer: &mut char, token: &wstr| {
        let mut expanded = token.to_owned();
        if expand_one(&mut expanded, ExpandFlags::FAIL_ON_CMDSUBST, ctx, None)
            && wstat(&expanded).is_ok_and(|md| md.is_dir())
        {
            *trailer = '/';
        }
    };

    if do_replace_token {
        if is_variable_name {
            assert!(!do_escape);
            if let Some(trailer) = trailer.as_mut() {
                maybe_add_slash(trailer, val_str);
            }
        }
        let mut move_cursor = 0;
        let (range, _) = parse_util_token_extent(command_line, cursor_pos);

        let mut sb = command_line[..range.start].to_owned();

        if keep_variable_override {
            let tok = &command_line[range.clone()];
            let separator_pos = variable_assignment_equals_pos(tok).unwrap();
            let key = &tok[..=separator_pos];
            sb.push_utfstr(&key);
            move_cursor += key.len();
        }

        if do_escape {
            let escaped = escape_string(val_str, EscapeStringStyle::Script(escape_flags));
            sb.push_utfstr(&escaped);
            move_cursor += escaped.len();
        } else {
            sb.push_utfstr(val_str);
            move_cursor += val_str.len();
        }

        if let Some(trailer) = trailer {
            if !have_trailer {
                sb.push(trailer);
            }
            move_cursor += 1;
        }
        sb.push_utfstr(&command_line[range.end..]);

        let new_cursor_pos = range.start + move_cursor;
        *inout_cursor_pos = new_cursor_pos;
        return sb;
    }

    let mut quote = None;
    let replaced = if do_escape {
        let (tok, _) = parse_util_token_extent(command_line, cursor_pos);
        // Find the last quote in the token to complete.
        let mut have_token = false;
        if tok.contains(&cursor_pos) || cursor_pos == tok.end {
            quote = get_quote(&command_line[tok.clone()], cursor_pos - tok.start);
            have_token = !tok.is_empty();
        }

        // If the token is reported as unquoted, but ends with a (unescaped) quote, and we can
        // modify the command line, then delete the trailing quote so that we can insert within
        // the quotes instead of after them. See issue #552.
        if quote.is_none() && !append_only && cursor_pos > 0 {
            // The entire token is reported as unquoted...see if the last character is an
            // unescaped quote.
            let trailing_quote = unescaped_quote(command_line, cursor_pos - 1);
            if trailing_quote.is_some() {
                quote = trailing_quote;
                back_into_trailing_quote = true;
            }
        }

        if have_token {
            escape_flags.insert(EscapeFlags::NO_QUOTED);
        }

        parse_util_escape_string_with_quote(val_str, quote, escape_flags)
    } else {
        val_str.to_owned()
    };

    let mut insertion_point = cursor_pos;
    if back_into_trailing_quote {
        // Move the character back one so we enter the terminal quote.
        insertion_point = insertion_point.checked_sub(1).unwrap();
    }

    // Perform the insertion and compute the new location.
    let mut result = command_line.to_owned();
    result.insert_utfstr(insertion_point, &replaced);
    let mut new_cursor_pos =
        insertion_point + replaced.len() + if back_into_trailing_quote { 1 } else { 0 };
    if let Some(mut trailer) = trailer {
        if is_variable_name {
            let (tok, _) = parse_util_token_extent(command_line, cursor_pos);
            maybe_add_slash(&mut trailer, &result[tok.start..new_cursor_pos]);
        }
        if trailer != '/'
            && quote.is_some()
            && unescaped_quote(command_line, insertion_point) != quote
        {
            // This is a quoted parameter, first print a quote.
            #[allow(clippy::unnecessary_unwrap)] // for old clippy
            result.insert(new_cursor_pos, quote.unwrap());
            new_cursor_pos += 1;
        }
        if !have_trailer {
            result.insert(new_cursor_pos, trailer);
        }
        new_cursor_pos += 1;
    }
    *inout_cursor_pos = new_cursor_pos;
    result
}

/// Check if the specified string can be replaced by a case insensitive completion with the
/// specified flags.
///
/// Advanced tokens like those containing {}-style expansion can not at the moment be replaced,
/// other than if the new token is already an exact replacement, e.g. if the COMPLETE_DONT_ESCAPE
/// flag is set.
fn reader_can_replace(s: &wstr, flags: CompleteFlags) -> bool {
    if flags.contains(CompleteFlags::DONT_ESCAPE) {
        return true;
    }

    // Test characters that have a special meaning in any character position.
    !s.chars()
        .any(|c| matches!(c, '$' | '*' | '?' | '(' | '{' | '}' | ')'))
}

/// Determine the best (lowest) match rank for a set of completions.
fn get_best_rank(comp: &[Completion]) -> u32 {
    let mut best_rank = u32::MAX;
    for c in comp {
        best_rank = best_rank.min(c.rank());
    }
    best_rank
}

impl<'a> Reader<'a> {
    /// Compute completions and update the pager and/or commandline as needed.
    fn compute_and_apply_completions(&mut self, c: ReadlineCmd) {
        assert!(matches!(
            c,
            ReadlineCmd::Complete | ReadlineCmd::CompleteAndSearch
        ));
        assert!(
            !get_tty_protocols_active(),
            "should not be called with TTY protocols active"
        );

        // Remove a trailing backslash. This may trigger an extra repaint, but this is
        // rare.
        let el = &self.command_line;
        if is_backslashed(el.text(), el.position()) {
            self.delete_char(true);
        }

        // Figure out the extent of the command substitution surrounding the cursor.
        // This is because we only look at the current command substitution to form
        // completions - stuff happening outside of it is not interesting.
        let el = &self.command_line;
        let cmdsub_range = parse_util_cmdsubst_extent(el.text(), el.position());
        let position_in_cmdsub = el.position() - cmdsub_range.start;

        // Figure out the extent of the token within the command substitution. Note we
        // pass cmdsub_begin here, not buff.
        let (mut token_range, _) =
            parse_util_token_extent(&el.text()[cmdsub_range.clone()], position_in_cmdsub);
        let position_in_token = position_in_cmdsub - token_range.start;

        // Hack: the token may extend past the end of the command substitution, e.g. in
        // (echo foo) the last token is 'foo)'. Don't let that happen.
        if token_range.end > cmdsub_range.len() {
            token_range.end = cmdsub_range.len();
        }
        token_range.start += cmdsub_range.start;
        token_range.end += cmdsub_range.start;

        // Check if we have a wildcard within this string; if so we first attempt to expand the
        // wildcard; if that succeeds we don't then apply user completions (#8593).
        let mut wc_expanded = WString::new();
        match try_expand_wildcard(
            self.parser,
            el.text()[token_range.clone()].to_owned(),
            position_in_token,
            &mut wc_expanded,
        ) {
            ExpandResultCode::error => {}
            ExpandResultCode::overflow => {
                // This may come about if we exceeded the max number of matches.
                // Return "success" to suppress normal completions.
                self.flash(token_range);
                return;
            }
            ExpandResultCode::wildcard_no_match => {}
            ExpandResultCode::cancel => {
                // e.g. the user hit control-C. Suppress normal completions.
                return;
            }
            ExpandResultCode::ok => {
                self.rls_mut().comp.clear();
                self.rls_mut().complete_did_insert = false;
                self.push_edit(
                    EditableLineTag::Commandline,
                    Edit::new(token_range, wc_expanded),
                );
                return;
            }
        }

        // Construct a copy of the string from the beginning of the command substitution
        // up to the end of the token we're completing.
        let cmdsub = &el.text()[cmdsub_range.start..token_range.end];

        let (comp, _needs_load) = complete(
            cmdsub,
            CompletionRequestOptions::normal(),
            &self.parser.context(),
        );
        self.rls_mut().comp = comp;

        let el = &self.command_line;
        // User-supplied completions may have changed the commandline - prevent buffer
        // overflow.
        token_range.start = std::cmp::min(token_range.start, el.text().len());
        token_range.end = std::cmp::min(token_range.end, el.text().len());

        // Munge our completions.
        sort_and_prioritize(
            &mut self.rls_mut().comp,
            CompletionRequestOptions::default(),
        );

        let el = &self.command_line;
        // Record our cycle_command_line.
        self.cycle_command_line = el.text().to_owned();
        self.cycle_cursor_pos = token_range.end;

        self.rls_mut().complete_did_insert = self.handle_completions(token_range);

        // Show the search field if requested and if we printed a list of completions.
        if c == ReadlineCmd::CompleteAndSearch
            && !self.rls().complete_did_insert
            && !self.pager.is_empty()
        {
            self.pager.set_search_field_shown(true);
            self.select_completion_in_direction(SelectionMotion::Next, false);
        }
    }

    fn try_insert(&mut self, c: Completion, tok: &wstr, token_range: Range<usize>) {
        // If this is a replacement completion, check that we know how to replace it, e.g. that
        // the token doesn't contain evil operators like {}.
        if !c.flags.contains(CompleteFlags::REPLACES_TOKEN) || reader_can_replace(tok, c.flags) {
            self.completion_insert(
                &c.completion,
                token_range.end,
                c.flags,
                /*is_unique=*/ true,
            );
        }
    }

    /// Handle the list of completions. This means the following:
    ///
    /// - If the list is empty, flash the terminal.
    /// - If the list contains one element, write the whole element, and if the element does not end on
    /// a '/', '@', ':', '.', ',', '-' or a '=', also write a trailing space.
    /// - If the list contains multiple elements, insert their common prefix, if any and display
    /// the list in the pager.  Depending on terminal size and the length of the list, the pager
    /// may either show less than a screenfull and exit or use an interactive pager to allow the
    /// user to scroll through the completions.
    ///
    /// \param comp the list of completion strings
    /// \param token_begin the position of the token to complete
    /// \param token_end the position after the token to complete
    ///
    /// Return true if we inserted text into the command line, false if we did not.
    fn handle_completions(&mut self, token_range: Range<usize>) -> bool {
        let tok = self.command_line.text()[token_range.clone()].to_owned();

        let comp = &self.rls().comp;
        // Check trivial cases.
        let len = comp.len();
        if len == 0 {
            // No suitable completions found, flash screen and return.
            if token_range.is_empty() {
                self.flash(0..self.command_line.len());
            } else {
                self.flash(token_range);
            }
            return false;
        } else if len == 1 {
            // Exactly one suitable completion found - insert it.
            let c = &comp[0];
            self.try_insert(c.clone(), &tok, token_range);
            return true;
        }

        let best_rank = get_best_rank(comp);

        // Determine whether we are going to replace the token or not. If any commands of the best
        // rank do not require replacement, then ignore all those that want to use replacement.
        let mut will_replace_token = true;
        for c in comp {
            if c.rank() <= best_rank && !c.flags.contains(CompleteFlags::REPLACES_TOKEN) {
                will_replace_token = false;
                break;
            }
        }

        // Decide which completions survived. There may be a lot of them; it would be nice if we could
        // figure out how to avoid copying them here.
        let mut surviving_completions = vec![];
        let mut all_matches_exact_or_prefix = true;
        for c in comp {
            // Ignore completions with a less suitable match rank than the best.
            if c.rank() > best_rank {
                continue;
            }

            // Only use completions that match replace_token.
            let completion_replaces_token = c.flags.contains(CompleteFlags::REPLACES_TOKEN);
            if completion_replaces_token != will_replace_token {
                continue;
            }

            // Don't use completions that want to replace, if we cannot replace them.
            if completion_replaces_token && !reader_can_replace(&tok, c.flags) {
                continue;
            }

            // This completion survived.
            surviving_completions.push(c.clone());
            all_matches_exact_or_prefix =
                all_matches_exact_or_prefix && c.r#match.is_exact_or_prefix();
        }

        if surviving_completions.len() == 1 {
            // After sorting and stuff only one completion is left, use it.
            //
            // TODO: This happens when smartcase kicks in, e.g.
            // the token is "cma" and the options are "cmake/" and "CMakeLists.txt"
            // it would be nice if we could figure
            // out how to use it more.
            let c = std::mem::take(&mut surviving_completions[0]);

            self.try_insert(c, &tok, token_range);
            return true;
        }

        let mut use_prefix = false;
        let mut common_prefix = L!("").to_owned();
        if all_matches_exact_or_prefix {
            // Try to find a common prefix to insert among the surviving completions.
            let mut flags = CompleteFlags::empty();
            let mut prefix_is_partial_completion = false;
            let mut first = true;
            for c in &surviving_completions {
                if first {
                    // First entry, use the whole string.
                    common_prefix = c.completion.clone();
                    flags = c.flags;
                    first = false;
                } else {
                    // Determine the shared prefix length.
                    let max = std::cmp::min(common_prefix.len(), c.completion.len());
                    let mut idx = 0;
                    while idx < max {
                        if common_prefix.as_char_slice()[idx] != c.completion.as_char_slice()[idx] {
                            break;
                        }
                        idx += 1;
                    }

                    // idx is now the length of the new common prefix.
                    common_prefix.truncate(idx);
                    prefix_is_partial_completion = true;

                    // Early out if we decide there's no common prefix.
                    if idx == 0 {
                        break;
                    }
                }
            }

            // Determine if we use the prefix. We use it if it's non-empty and it will actually make
            // the command line longer. It may make the command line longer by virtue of not using
            // REPLACE_TOKEN (so it always appends to the command line), or by virtue of replacing
            // the token but being longer than it.
            use_prefix = common_prefix.len() > if will_replace_token { tok.len() } else { 0 };
            assert!(!use_prefix || !common_prefix.is_empty());

            if use_prefix {
                // We got something. If more than one completion contributed, then it means we have
                // a prefix; don't insert a space after it.
                if prefix_is_partial_completion {
                    flags |= CompleteFlags::NO_SPACE;
                }
                self.completion_insert(
                    &common_prefix,
                    token_range.end,
                    flags,
                    /*is_unique=*/ false,
                );
                self.cycle_command_line = self.command_line.text().to_owned();
                self.cycle_cursor_pos = self.command_line.position();
            }
        }

        if use_prefix {
            for c in &mut surviving_completions {
                c.flags &= !CompleteFlags::REPLACES_TOKEN;
                c.completion.replace_range(0..common_prefix.len(), L!(""));
            }
        }

        // Print the completion list.
        let mut prefix = WString::new();
        if will_replace_token || !all_matches_exact_or_prefix {
            if use_prefix {
                prefix.push_utfstr(&common_prefix);
            }
        } else if tok.len() + common_prefix.len() <= PREFIX_MAX_LEN {
            prefix.push_utfstr(&tok);
            prefix.push_utfstr(&common_prefix);
        } else {
            // Collapse parent directories and append end of string
            prefix.push(get_ellipsis_char());

            let full = tok + &common_prefix[..];
            let truncated = &full[full.len() - PREFIX_MAX_LEN..];
            let (i, last_component) = truncated.split('/').enumerate().last().unwrap();
            if i == 0 {
                // No path separators were found in the common prefix, so we can't collapse
                // any further
                prefix.push_utfstr(&truncated);
            } else {
                // Discard any parent directories and include whats left
                prefix.push('/');
                prefix.push_utfstr(last_component);
            };
        }

        // Update the pager data.
        self.pager.set_prefix(&prefix, true);
        self.pager.set_completions(&surviving_completions, true);
        // Modify the command line to reflect the new pager.
        self.pager_selection_changed();
        false
    }

    /// Insert the string at the current cursor position. The function checks if the string is quoted or
    /// not and correctly escapes the string.
    ///
    /// \param val the string to insert
    /// \param token_end the position after the token to complete
    /// \param flags A union of all flags describing the completion to insert. See the completion_t
    /// struct for more information on possible values.
    fn completion_insert(
        &mut self,
        val: &wstr,
        token_end: usize,
        flags: CompleteFlags,
        is_unique: bool,
    ) {
        let (elt, el) = self.active_edit_line();

        // Move the cursor to the end of the token.
        if el.position() != token_end {
            self.update_buff_pos(elt, Some(token_end));
        }

        let (_elt, el) = self.active_edit_line();
        let mut cursor = el.position();
        let new_command_line = completion_apply_to_command_line(
            &OperationContext::background_interruptible(self.parser.vars()),
            val,
            flags,
            el.text(),
            &mut cursor,
            /*append_only=*/ false,
            is_unique,
        );
        self.set_buffer_maintaining_pager(&new_command_line, cursor);
    }
}
