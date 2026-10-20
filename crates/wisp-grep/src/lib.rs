/*!
The main entry point into ripgrep.
*/

use std::{ffi::OsString, io::Write, path::Path, path::PathBuf, process::ExitCode};

use ignore::WalkState;

use crate::flags::{HiArgs, SearchMode};

#[macro_use]
#[path = "rg/messages.rs"]
mod messages;

#[path = "rg/flags/mod.rs"]
mod flags;
#[path = "rg/haystack.rs"]
mod haystack;
#[path = "index/mod.rs"]
mod index;
#[path = "rg/logger.rs"]
mod logger;
#[path = "rg/search.rs"]
mod search;

// Since Rust no longer uses jemalloc by default, ripgrep will, by default,
// use the system allocator. On Linux, this would normally be glibc's
// allocator, which is pretty good. In particular, ripgrep does not have a
// particularly allocation heavy workload, so there really isn't much
// difference (for ripgrep's purposes) between glibc's allocator and jemalloc.
//
// However, when ripgrep is built with musl, this means ripgrep will use musl's
// allocator, which appears to be substantially worse. (musl's goal is not to
// have the fastest version of everything. Its goal is to be small and amenable
// to static compilation.) Even though ripgrep isn't particularly allocation
// heavy, musl's allocator appears to slow down ripgrep quite a bit. Therefore,
// when building with musl, we use jemalloc.
//
// We don't unconditionally use jemalloc because it can be nice to use the
// system's default allocator by default. Moreover, jemalloc seems to increase
// compilation times by a bit.
//
// Moreover, we only do this on 64-bit systems since jemalloc doesn't support
// i686.
#[cfg(all(target_env = "musl", target_pointer_width = "64"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Run the `wgrep` binary: index management first, then one rg invocation.
pub fn run_cli() -> ExitCode {
    index::profile::mark("main");
    // wgrep: index management commands are handled before rg's parser.
    if let Some(code) = index::cli::run_if_requested() {
        return code;
    }
    index::events::set_walk_only(false);
    messages::reset_errored();
    let parsed = flags::parse();
    finish(parsed)
}

/// Run one vendored-ripgrep invocation with an explicit argument list.
///
/// This behaves exactly like the `wgrep` binary in one-shot (no index, no
/// daemon) mode. It uses the process stdin, stdout, stderr and working
/// directory. The return value is ripgrep's exit code.
pub fn run_rg(argv: Vec<OsString>) -> i32 {
    index::events::set_walk_only(true);
    messages::reset_errored();
    let parsed = flags::parse_from(argv);
    exit_value(finish(parsed))
}

/// List the files ripgrep would search for `path` under `cwd`.
///
/// This uses the same walker and default ignore rules as
/// `rg --no-config --files`. `path` is spelled as ripgrep would see it and
/// returned paths keep ripgrep's spelling. The walk is in-process; it does not
/// run an `rg` binary and does not use an index.
pub fn list_files(cwd: &Path, path: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else if path == Path::new(".") || path == Path::new("./") {
        cwd.to_path_buf()
    } else {
        cwd.join(path)
    };
    let parsed = flags::parse_args_at(
        vec![
            OsString::from("--files"),
            OsString::from("-0"),
            OsString::from("--"),
            absolute.clone().into_os_string(),
        ],
        cwd.to_path_buf(),
    );
    let args = match parsed {
        flags::ParseResult::Ok(args) => args,
        flags::ParseResult::Err(err) => return Err(err),
        flags::ParseResult::Special(_) => anyhow::bail!("unexpected special mode"),
    };
    let haystack_builder = args.haystack_builder();
    let mut files = Vec::new();
    for result in args.walk_builder()?.build() {
        let dent = result.map_err(|err| anyhow::anyhow!("{err}"))?;
        let Some(haystack) = haystack_builder.build_from_result(Ok(dent)) else { continue };
        let walked = haystack.path();
        let relative = walked.strip_prefix(&absolute).unwrap_or(walked);
        files.push(path.join(relative));
    }
    Ok(files)
}

fn exit_value(code: ExitCode) -> i32 {
    if code == ExitCode::SUCCESS {
        0
    } else if code == ExitCode::from(1) {
        1
    } else {
        2
    }
}

/// Then, as it was, then again it will be.
fn finish(parsed: crate::flags::ParseResult<HiArgs>) -> ExitCode {
    index::profile::mark("args_parsed");
    let code = match run(parsed) {
        Ok(code) => code,
        Err(err) => {
            // Look for a broken pipe error. In this case, we generally want
            // to exit "gracefully" with a success exit code. This matches
            // existing Unix convention. We need to handle this explicitly
            // since the Rust runtime doesn't ask for PIPE signals, and thus
            // we get an I/O error instead. Traditional C Unix applications
            // quit by getting a PIPE signal that they don't handle, and thus
            // the unhandled signal causes the process to unceremoniously
            // terminate.
            let broken_pipe = err.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|ioerr| ioerr.kind() == std::io::ErrorKind::BrokenPipe)
            });
            if broken_pipe {
                ExitCode::from(0)
            } else {
                eprintln_locked!("{:#}", err);
                ExitCode::from(2)
            }
        }
    };
    index::profile::flush("client");
    code
}

/// The main entry point for ripgrep.
///
/// The given parse result determines ripgrep's behavior. The parse
/// result should be the result of parsing CLI arguments in a low level
/// representation, and then followed by an attempt to convert them into a
/// higher level representation. The higher level representation has some nicer
/// abstractions, for example, instead of representing the `-g/--glob` flag
/// as a `Vec<String>` (as in the low level representation), the globs are
/// converted into a single matcher.
fn run(result: crate::flags::ParseResult<HiArgs>) -> anyhow::Result<ExitCode> {
    use crate::flags::{Mode, ParseResult};

    let args = match result {
        ParseResult::Err(err) => return Err(err),
        ParseResult::Special(mode) => return special(mode),
        ParseResult::Ok(args) => args,
    };
    let matched = match args.mode() {
        Mode::Search(_) if !args.matches_possible() => false,
        Mode::Search(mode) if args.threads() == 1 => search(&args, mode)?,
        Mode::Search(mode) => search_parallel(&args, mode)?,
        Mode::Files if args.threads() == 1 => files(&args)?,
        Mode::Files => files_parallel(&args)?,
        Mode::Types => return types(&args),
        Mode::Generate(mode) => return generate(mode),
    };
    Ok(if matched && (args.quiet() || !messages::errored()) {
        ExitCode::from(0)
    } else if messages::errored() {
        ExitCode::from(2)
    } else {
        ExitCode::from(1)
    })
}

/// The top-level entry point for single-threaded search.
///
/// This recursively steps through the file list (current directory by default)
/// and searches each file sequentially.
fn search(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {
    let started_at = std::time::Instant::now();
    let haystack_builder = args.haystack_builder();
    // wgrep: a running index daemon replaces the walk when it applies.
    let daemon = args.index_daemon_haystacks();
    index::profile::mark("daemon_haystacks_done");
    index::profile::set("threads", serde_json::json!(1));
    if let Some(d) = daemon.as_ref() {
        index::profile::set("haystacks", serde_json::json!(d.haystacks.len()));
    }
    let daemon_present = daemon.is_some();
    let mut searched = daemon.as_ref().is_some_and(|d| d.any_files);
    // Held until this function returns; dropping a lease releases it.
    let (unsorted, _leases): (Box<dyn Iterator<Item = haystack::Haystack>>, _) = match daemon {
        Some(d) => (Box::new(d.haystacks.into_iter()), d.leases),
        None => (
            Box::new(args.walk_builder()?.build().filter_map(|result| haystack_builder.build_from_result(result))),
            Vec::new(),
        ),
    };
    let haystacks = args.sort(unsorted);

    let mut matched = false;
    let mut stats = args.stats();
    let mut searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, args.stdout()),
    )?;
    let pruner = if searched { None } else { args.index_pruner() };
    if !daemon_present {
        crate::index::events::record_walk(
            if pruner.is_some() { "prune" } else { "walk" },
            "daemon_disabled",
        );
    }
    for haystack in haystacks {
        searched = true;
        if pruner.as_ref().is_some_and(|p| haystack.dent().is_some_and(|d| p.skip(d))) {
            continue;
        }
        let search_result = match searcher.search(&haystack) {
            Ok(search_result) => search_result,
            // A broken pipe means graceful termination.
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(err) => {
                err_message!("{}: {}", haystack.path().display(), err);
                continue;
            }
        };
        matched = matched || search_result.has_match();
        if let Some(ref mut stats) = stats {
            *stats += search_result.stats().unwrap();
        }
        if matched && args.quit_after_match() {
            break;
        }
    }
    index::profile::mark("search_done");
    if args.has_implicit_path() && !searched {
        eprint_nothing_searched();
    }
    if let Some(ref stats) = stats {
        let wtr = searcher.printer().get_mut();
        let _ = print_stats(mode, stats, started_at, wtr);
    }
    Ok(matched)
}

/// The top-level entry point for multi-threaded search.
///
/// The parallelism is itself achieved by the recursive directory traversal.
/// All we need to do is feed it a worker for performing a search on each file.
///
/// Requesting a sorted output from ripgrep (such as with `--sort path`) will
/// automatically disable parallelism and hence sorting is not handled here.
fn search_parallel(args: &HiArgs, mode: SearchMode) -> anyhow::Result<bool> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let started_at = std::time::Instant::now();
    let haystack_builder = args.haystack_builder();
    let bufwtr = args.buffer_writer();
    let stats = args.stats().map(std::sync::Mutex::new);
    let matched = AtomicBool::new(false);
    let searched = AtomicBool::new(false);

    let mut searcher = args.search_worker(
        args.matcher()?,
        args.searcher()?,
        args.printer(mode, bufwtr.buffer()),
    )?;
    // wgrep: the body of rg's per-file walk callback.
    let search_one = |searcher: &mut search::SearchWorker<termcolor::Buffer>,
                      haystack: haystack::Haystack|
     -> WalkState {
        // wgrep: this runs once per searched file; `searched` is read only
        // after the walk joins, so a relaxed load skips the store once set
        // and avoids cache-line ping-pong between the walker threads.
        if !searched.load(Ordering::Relaxed) {
            searched.store(true, Ordering::Relaxed);
        }
        searcher.printer().get_mut().clear();
        let search_result = match searcher.search(&haystack) {
            Ok(search_result) => search_result,
            Err(err) => {
                err_message!("{}: {}", haystack.path().display(), err);
                return WalkState::Continue;
            }
        };
        if search_result.has_match() {
            matched.store(true, Ordering::SeqCst);
        }
        if let Some(ref locked_stats) = stats {
            let mut stats = locked_stats.lock().unwrap();
            *stats += search_result.stats().unwrap();
        }
        if let Err(err) = bufwtr.print(searcher.printer().get_mut()) {
            // A broken pipe means graceful termination.
            if err.kind() == std::io::ErrorKind::BrokenPipe {
                return WalkState::Quit;
            }
            // Otherwise, we continue on our merry way.
            err_message!("{}: {}", haystack.path().display(), err);
        }
        if matched.load(Ordering::SeqCst) && args.quit_after_match() {
            WalkState::Quit
        } else {
            WalkState::Continue
        }
    };
    let daemon = args.index_daemon_haystacks();
    index::profile::mark("daemon_haystacks_done");
    if let Some(daemon) = daemon {
        // A running daemon already resolved candidates; search those instead
        // of walking. Its thread lease bounds broad searches under load.
        let haystacks = &daemon.haystacks;
        searched.store(daemon.any_files, Ordering::Relaxed);
        index::profile::set("haystacks", serde_json::json!(haystacks.len()));
        if !haystacks.is_empty() {
            let quit = AtomicBool::new(false);
            let threads = daemon.threads.map_or(args.threads(), |t| t.min(args.threads())).min(haystacks.len()).max(1);
            index::profile::set("threads", serde_json::json!(threads));
            let next = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    let mut searcher = searcher.clone();
                    let (haystacks, next, quit, search_one) = (haystacks, &next, &quit, &search_one);
                    scope.spawn(move || {
                        while !quit.load(Ordering::SeqCst) {
                            let Some(haystack) = haystacks.get(next.fetch_add(1, Ordering::SeqCst)) else { break };
                            if matches!(search_one(&mut searcher, haystack.clone()), WalkState::Quit) {
                                quit.store(true, Ordering::SeqCst);
                            }
                        }
                    });
                }
            });
        }
    } else {
        let pruner = args.index_pruner();
        crate::index::events::record_walk(
            if pruner.is_some() { "prune" } else { "walk" },
            "daemon_disabled",
        );
        index::profile::set("threads", serde_json::json!(args.threads()));
        args.walk_builder()?.build_parallel().run(|| {
            let haystack_builder = &haystack_builder;
            let pruner = &pruner;
            let searched = &searched;
            let search_one = &search_one;
            let mut searcher = searcher.clone();

            Box::new(move |result| {
                // wgrep: decide on the walk entry before building the haystack,
                // so a skipped file never pays for the haystack's type/symlink
                // logic.
                let dent = match result {
                    Ok(dent) => dent,
                    Err(err) => {
                        err_message!("{err}");
                        return WalkState::Continue;
                    }
                };
                if pruner.as_ref().is_some_and(|p| p.skip(&dent)) {
                    if !searched.load(Ordering::Relaxed) {
                        searched.store(true, Ordering::Relaxed);
                    }
                    return WalkState::Continue;
                }
                let Some(haystack) = haystack_builder.build_from_result(Ok(dent)) else {
                    return WalkState::Continue;
                };
                search_one(&mut searcher, haystack)
            })
        });
    }
    index::profile::mark("search_done");
    if args.has_implicit_path() && !searched.load(Ordering::Relaxed) {
        eprint_nothing_searched();
    }
    if let Some(ref locked_stats) = stats {
        let stats = locked_stats.lock().unwrap();
        let mut wtr = searcher.printer().get_mut();
        let _ = print_stats(mode, &stats, started_at, &mut wtr);
        let _ = bufwtr.print(&mut wtr);
    }
    Ok(matched.load(Ordering::SeqCst))
}

/// The top-level entry point for file listing without searching.
///
/// This recursively steps through the file list (current directory by default)
/// and prints each path sequentially using a single thread.
fn files(args: &HiArgs) -> anyhow::Result<bool> {
    let haystack_builder = args.haystack_builder();
    let unsorted = args
        .walk_builder()?
        .build()
        .filter_map(|result| haystack_builder.build_from_result(result));
    let haystacks = args.sort(unsorted);

    let mut matched = false;
    let mut path_printer = args.path_printer_builder().build(args.stdout());
    for haystack in haystacks {
        matched = true;
        if args.quit_after_match() {
            break;
        }
        if let Err(err) = path_printer.write(haystack.path()) {
            // A broken pipe means graceful termination.
            if err.kind() == std::io::ErrorKind::BrokenPipe {
                break;
            }
            // Otherwise, we have some other error that's preventing us from
            // writing to stdout, so we should bubble it up.
            return Err(err.into());
        }
    }
    Ok(matched)
}

/// The top-level entry point for multi-threaded file listing without
/// searching.
///
/// This recursively steps through the file list (current directory by default)
/// and prints each path sequentially using multiple threads.
///
/// Requesting a sorted output from ripgrep (such as with `--sort path`) will
/// automatically disable parallelism and hence sorting is not handled here.
fn files_parallel(args: &HiArgs) -> anyhow::Result<bool> {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
    };

    let haystack_builder = args.haystack_builder();
    let mut path_printer = args.path_printer_builder().build(args.stdout());
    let matched = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<crate::haystack::Haystack>();

    // We spawn a single printing thread to make sure we don't tear writes.
    // We use a channel here under the presumption that it's probably faster
    // than using a mutex in the worker threads below, but this has never been
    // seriously litigated.
    let print_thread = thread::spawn(move || -> std::io::Result<()> {
        for haystack in rx.iter() {
            path_printer.write(haystack.path())?;
        }
        Ok(())
    });
    args.walk_builder()?.build_parallel().run(|| {
        let haystack_builder = &haystack_builder;
        let matched = &matched;
        let tx = tx.clone();

        Box::new(move |result| {
            let haystack = match haystack_builder.build_from_result(result) {
                Some(haystack) => haystack,
                None => return WalkState::Continue,
            };
            matched.store(true, Ordering::SeqCst);
            if args.quit_after_match() {
                WalkState::Quit
            } else {
                match tx.send(haystack) {
                    Ok(_) => WalkState::Continue,
                    Err(_) => WalkState::Quit,
                }
            }
        })
    });
    drop(tx);
    if let Err(err) = print_thread.join().unwrap() {
        // A broken pipe means graceful termination, so fall through.
        // Otherwise, something bad happened while writing to stdout, so bubble
        // it up.
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            return Err(err.into());
        }
    }
    Ok(matched.load(Ordering::SeqCst))
}

/// The top-level entry point for `--type-list`.
fn types(args: &HiArgs) -> anyhow::Result<ExitCode> {
    let mut count = 0;
    let mut stdout = args.stdout();
    for def in args.types().definitions() {
        count += 1;
        stdout.write_all(def.name().as_bytes())?;
        stdout.write_all(b": ")?;

        let mut first = true;
        for glob in def.globs() {
            if !first {
                stdout.write_all(b", ")?;
            }
            stdout.write_all(glob.as_bytes())?;
            first = false;
        }
        stdout.write_all(b"\n")?;
    }
    Ok(ExitCode::from(if count == 0 { 1 } else { 0 }))
}

/// Implements ripgrep's "generate" modes.
///
/// These modes correspond to generating some kind of ancillary data related
/// to ripgrep. At present, this includes ripgrep's man page (in roff format)
/// and supported shell completions.
fn generate(mode: crate::flags::GenerateMode) -> anyhow::Result<ExitCode> {
    use crate::flags::GenerateMode;

    let output = match mode {
        GenerateMode::Man => flags::generate_man_page(),
        GenerateMode::CompleteBash => flags::generate_complete_bash(),
        GenerateMode::CompleteZsh => flags::generate_complete_zsh(),
        GenerateMode::CompleteFish => flags::generate_complete_fish(),
        GenerateMode::CompletePowerShell => {
            flags::generate_complete_powershell()
        }
    };
    writeln!(std::io::stdout(), "{}", output.trim_end())?;
    Ok(ExitCode::from(0))
}

/// Implements ripgrep's "special" modes.
///
/// A special mode is one that generally short-circuits most (not all) of
/// ripgrep's initialization logic and skips right to this routine. The
/// special modes essentially consist of printing help and version output. The
/// idea behind the short circuiting is to ensure there is as little as possible
/// (within reason) that would prevent ripgrep from emitting help output.
///
/// For example, part of the initialization logic that is skipped (among
/// other things) is accessing the current working directory. If that fails,
/// ripgrep emits an error. We don't want to emit an error if it fails and
/// the user requested version or help information.
fn special(mode: crate::flags::SpecialMode) -> anyhow::Result<ExitCode> {
    use crate::flags::SpecialMode;

    let mut exit = ExitCode::from(0);
    let output = match mode {
        SpecialMode::HelpShort => flags::generate_help_short(),
        SpecialMode::HelpLong => flags::generate_help_long(),
        SpecialMode::VersionShort => flags::generate_version_short(),
        SpecialMode::VersionLong => flags::generate_version_long(),
        // --pcre2-version is a little special because it emits an error
        // exit code if this build of ripgrep doesn't support PCRE2.
        SpecialMode::VersionPCRE2 => {
            let (output, available) = flags::generate_version_pcre2();
            if !available {
                exit = ExitCode::from(1);
            }
            output
        }
    };
    writeln!(std::io::stdout(), "{}", output.trim_end())?;
    Ok(exit)
}

/// Prints a heuristic error messages when nothing is searched.
///
/// This can happen if an applicable ignore file has one or more rules that
/// are too broad and cause ripgrep to ignore everything.
///
/// We only show this error message when the user does *not* provide an
/// explicit path to search. This is because the message can otherwise be
/// noisy, e.g., when it is intended that there is nothing to search.
fn eprint_nothing_searched() {
    err_message!(
        "No files were searched, which means ripgrep probably \
         applied a filter you didn't expect.\n\
         Running with --debug will show why files are being skipped."
    );
}

/// Prints the statistics given to the writer given.
///
/// The search mode given determines whether the stats should be printed in
/// a plain text format or in a JSON format.
///
/// The `started` time should be the time at which ripgrep started working.
///
/// If an error occurs while writing, then writing stops and the error is
/// returned. Note that callers should probably ignore this errror, since
/// whether stats fail to print or not generally shouldn't cause ripgrep to
/// enter into an "error" state. And usually the only way for this to fail is
/// if writing to stdout itself fails.
fn print_stats<W: Write>(
    mode: SearchMode,
    stats: &grep::printer::Stats,
    started: std::time::Instant,
    mut wtr: W,
) -> std::io::Result<()> {
    let elapsed = std::time::Instant::now().duration_since(started);
    if matches!(mode, SearchMode::JSON) {
        // We specifically match the format laid out by the JSON printer in
        // the grep-printer crate. We simply "extend" it with the 'summary'
        // message type.
        serde_json::to_writer(
            &mut wtr,
            &serde_json::json!({
                "type": "summary",
                "data": {
                    "stats": stats,
                    "elapsed_total": {
                        "secs": elapsed.as_secs(),
                        "nanos": elapsed.subsec_nanos(),
                        "human": format!("{:0.6}s", elapsed.as_secs_f64()),
                    },
                }
            }),
        )?;
        write!(wtr, "\n")
    } else {
        write!(
            wtr,
            "
{matches} matches
{lines} matched lines
{searches_with_match} files contained matches
{searches} files searched
{bytes_printed} bytes printed
{bytes_searched} bytes searched
{search_time:0.6} seconds spent searching
{process_time:0.6} seconds total
",
            matches = stats.matches(),
            lines = stats.matched_lines(),
            searches_with_match = stats.searches_with_match(),
            searches = stats.searches(),
            bytes_printed = stats.bytes_printed(),
            bytes_searched = stats.bytes_searched(),
            search_time = stats.elapsed().as_secs_f64(),
            process_time = elapsed.as_secs_f64(),
        )
    }
}
