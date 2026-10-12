mod agg;
mod eval;
mod exec;
mod json;
mod output;
mod plan;
mod query;

use anyhow::{Context, Result};
use clap::Parser;
use exec::{Ctx, Source};
use plan::Format;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "lq", version, about = "Fast query / filter / aggregate for JSON-Lines logs", after_help = query::HELP)]
struct Cli {
    /// Query pipeline, e.g. 'level == "error" | count by service' ("" = every record)
    query: String,
    /// Input files ("-" or none = stdin)
    files: Vec<PathBuf>,
    /// Follow files as they grow (survives truncation and rotation). Aggregates re-render live
    #[arg(short, long)]
    follow: bool,
    /// With --follow: process existing content first instead of starting at the end
    #[arg(long)]
    from_start: bool,
    /// Output format
    #[arg(short, long, value_enum, default_value_t = Format::Auto)]
    output: Format,
    /// Worker threads (default: number of CPUs)
    #[arg(short = 'j', long)]
    threads: Option<usize>,
    /// Seconds between re-renders of live aggregates
    #[arg(long, default_value_t = 1.0)]
    interval: f64,
    /// Disable the substring prefilter (for benchmarking the scanner)
    #[arg(long)]
    no_prefilter: bool,
    /// Print the compiled plan and exit
    #[arg(long)]
    explain: bool,
    /// Print throughput statistics to stderr
    #[arg(long)]
    stats: bool,
}

/// Exit status like grep: 0 = something matched, 1 = nothing matched, 2 = error.
fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("lq: {e:#}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    let plan = plan::compile(query::parse(&cli.query)?, cli.output)?;
    if cli.explain {
        println!("{plan:#?}");
        return Ok(true);
    }
    let threads = cli.threads.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get())).max(1);
    let mut ctx = Ctx::new(&plan, threads, !cli.no_prefilter)?;
    let started = Instant::now();

    let stdin = cli.files.is_empty() || cli.files.iter().any(|f| f.as_os_str() == "-");
    if cli.follow {
        if stdin {
            anyhow::bail!("--follow needs file paths; piped stdin is already processed live without it");
        }
        ctx.follow(&cli.files, cli.from_start, Duration::from_secs_f64(cli.interval.max(0.05)))?;
        unreachable!("follow only returns with an error");
    }
    if cli.files.is_empty() {
        ctx.run(Source::Reader(Box::new(std::io::stdin())))?;
    }
    for f in &cli.files {
        if f.as_os_str() == "-" {
            ctx.run(Source::Reader(Box::new(std::io::stdin())))?;
            continue;
        }
        let file = std::fs::File::open(f).with_context(|| format!("{}", f.display()))?;
        let meta = file.metadata()?;
        if meta.is_file() && meta.len() > 0 {
            // SAFETY: read-only mapping. If another process truncates the file while we read it,
            // we may see SIGBUS; that is the usual trade-off of mmap-based tools (ripgrep et al.).
            let map = unsafe { memmap2::Mmap::map(&file) }.with_context(|| format!("mmap {}", f.display()))?;
            let _ = map.advise(memmap2::Advice::Sequential);
            ctx.run(Source::Bytes(&map))?;
        } else if !meta.is_file() {
            ctx.run(Source::Reader(Box::new(file)))?; // pipes, /dev/stdin, process substitution
        }
    }
    ctx.finish()?;
    if cli.stats {
        let secs = started.elapsed().as_secs_f64();
        eprintln!(
            "lq: {} matched, {:.1} MB in {:.3}s ({:.2} GB/s, {} threads)",
            ctx.matched,
            ctx.bytes as f64 / 1e6,
            secs,
            ctx.bytes as f64 / 1e9 / secs,
            threads
        );
    }
    Ok(ctx.matched > 0)
}
