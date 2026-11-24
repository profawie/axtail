//! axtail - advanced tail: a fast, read-only terminal log viewer.

mod app;
mod clock;
mod filter;
mod hidden;
mod source;
mod summary;
mod ui;
mod window;

use std::process::ExitCode;

use filter::Filter;

const USAGE: &str = "\
axtail {v} - advanced tail for sysadmins (read-only log viewer)

USAGE:
    axtail [OPTIONS] <FILTER>...
    axtail --docker <CONTAINER> [OPTIONS] [FILTER]...

FILTER:
    A log file, a directory, or a wildcard such as *.log, *.zip, *.txt,
    api-*.log. Directories, .zip and .gz archives are scanned and every log
    inside is listed. Quote wildcards to let axtail expand them itself.

OPTIONS:
    --docker <CONTAINER>   Read a container's logs through `docker logs`, and
                           list log files inside the container, including
                           .gz and .zip archives. Without a FILTER the usual
                           log folders are searched (/var/log, /opt, /app,
                           /srv, /data, /home, the workdir, ...).
    --grep <PATTERN>       Advanced search: a case-insensitive regex that is
                           applied as the viewer filter, with the number of
                           matching lines shown for every file in the list.
    --tz <ZONE>            Time zone for dates shown and typed: UTC, local, or
                           an offset such as +08:00 (default: this machine's
                           zone; the AXTAIL_TZ variable works too). The zone
                           in use is shown at the bottom right.
    --tail <LINES>         Lines kept in memory for docker/compressed logs
                           (default 10000, max 200000).
    -h, --help             Show this help.
    -V, --version          Show the version.

EXAMPLES:
    axtail /var/log                      List every log in a directory
    axtail '*.log'                       Logs in the current directory
    axtail 'api-*.log' --grep 'ERROR|timeout'
    axtail archive.zip                   Logs inside a zip, never extracted to disk
    axtail app.log.gz                    Stream-decompressed gzip
    axtail --docker myapp                Live container logs
    axtail --docker myapp '/var/log/*.log'
    axtail --docker myapp '*.gz'           Archived logs anywhere in the usual folders
    axtail --docker myapp --grep 'panic'
    axtail --docker myapp --tz UTC       Dates in UTC, like most container logs

KEYS:
    File list   type to search names, Left/Right time range, Tab From/To dates,
                Up/Down + Enter to open
    Log viewer  type to filter lines (error timeout, +must, -skip, \"phrase\",
                /regex/), Up/Down scroll, Enter live/pause, Esc back
    Summary     Ctrl+D analyses the listed logs: lines per level and errors
                grouped into types; Tab next level, Enter details with every
                timestamp, Enter again opens the log at that line, Del hides a
                message type (saved in ~/.config/axtail/hidden.txt or
                %APPDATA%\\axtail\\hidden.txt; AXTAIL_HIDDEN sets another path)
    Anywhere    Ctrl+G help, Ctrl+X exit

TIME ZONE:
    Dates in the file list and the From / To fields use this machine's time
    zone, detected at start and shown at the bottom right. Use --tz UTC (or
    --tz +08:00) when the logs' own timestamps are in another zone.
";

struct Args {
    docker: Option<String>,
    grep: Option<String>,
    ignore_case: bool,
    tail: usize,
    tz: Option<String>,
    filters: Vec<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        docker: None,
        grep: None,
        ignore_case: false,
        tail: 10_000,
        tz: None,
        filters: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    let mut any = false;
    while let Some(a) = it.next() {
        any = true;
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                println!("axtail {}", ui::VERSION);
                std::process::exit(0);
            }
            "--docker" => args.docker = Some(value("--docker")?),
            "--grep" => args.grep = Some(value("--grep")?),
            "-i" | "--ignore-case" => args.ignore_case = true,
            "--tz" => args.tz = Some(value("--tz")?),
            "--tail" => {
                let n: usize = value("--tail")?
                    .parse()
                    .map_err(|_| "--tail needs a number".to_string())?;
                args.tail = n.clamp(100, 200_000);
            }
            "--" => args.filters.extend(it.by_ref()),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => args.filters.push(a),
        }
    }
    if !any || (args.docker.is_none() && args.filters.is_empty()) {
        return Ok(None);
    }
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{}", USAGE.replace("{v}", ui::VERSION));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("axtail: {e}\nTry 'axtail --help'.");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = clock::init(args.tz.as_deref()) {
        eprintln!("axtail: {e}");
        return ExitCode::from(2);
    }
    let grep = match &args.grep {
        None => None,
        Some(p) => match Filter::regex(p, &format!("/{p}/")) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("axtail: invalid --grep pattern: {e}");
                return ExitCode::from(2);
            }
        },
    };
    let _ = args.ignore_case; // accepted for compatibility: filters ignore case
    let scan = match &args.docker {
        Some(c) => {
            eprintln!("axtail: looking for log files in container {c}...");
            source::scan_docker(c, &args.filters)
        }
        None => source::scan_local(&args.filters),
    };
    if scan.sources.is_empty() {
        for w in &scan.warnings {
            eprintln!("axtail: {w}");
        }
        eprintln!("axtail: no log files found");
        return ExitCode::FAILURE;
    }
    let mut app = app::App::new(scan, grep, args.tail);
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    drop(app);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("axtail: {e}");
            ExitCode::FAILURE
        }
    }
}
