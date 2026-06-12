// bounce — a fast, zero-dependency file archiver.
//
// The name and codec are inspired by Big Bounce cosmology: data is collapsed
// (compressed) and later bounces back (decompressed) to its exact original
// state. Compression uses the Big Bounce smart-deflate codec (LZ77 + per-block
// Huffman with byte-shuffle transforms), selected automatically per file.
//
// Licensed under the Apache License, Version 2.0.
// Task-routing behavior is covered by the author's patent.

mod archive;
mod codec;

use std::process::ExitCode;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const EXT: &str = ".bnc";

fn print_usage() {
    println!(
        "bounce {VERSION} — Big Bounce file archiver

USAGE:
    bounce <command> [options] <archive> [files...]

COMMANDS:
    c, create    <archive{EXT}> <files...>   Create an archive from files/dirs
    x, extract   <archive{EXT}> [files...]   Extract files (all, or only named)
    l, list      <archive{EXT}>              List archive contents
    t, test      <archive{EXT}>              Verify archive integrity (CRC-32)
    h, help                                  Show this help
    v, version                               Show version

OPTIONS:
    -1 ... -9            Compression levels (default: -1):
                            -1: 64 KB window, 128 KB blocks (fastest)
                           -2: 128 KB window, 128 KB blocks
                           -3: 256 KB window, 256 KB blocks
                           -4: 512 KB window, 512 KB blocks
                           -5: 1 MB window, 1 MB blocks
                           -6: 2 MB window, 2 MB blocks
                           -7: 4 MB window, 4 MB blocks
                           -8: 8 MB window, 8 MB blocks
                           -9: 16 MB window, 16 MB blocks
                           -10+: scales exponentially up to available RAM
    -o, --output <dir>   Extract into <dir> (default: current directory)
    -c, --stdout         Write extracted file(s) to stdout instead of disk
    -f, --force          Overwrite existing files when extracting
    -v, --verbose        Show per-file progress
    -q, --quiet          Suppress the summary line

EXAMPLES:
    bounce c backup{EXT} report.pdf photos/        Create an archive
    bounce l backup{EXT}                           List contents
    bounce t backup{EXT}                           Check integrity
    bounce x backup{EXT} -o restored/              Extract everything
    bounce x backup{EXT} report.pdf                Extract a single file
    bounce x backup{EXT} report.pdf -c > out.pdf   Extract to stdout (pipe)

Compressed archives use the {EXT} extension."
    );
}

struct Options {
    output: String,
    to_stdout: bool,
    force: bool,
    verbose: bool,
    quiet: bool,
    level: u8,
    positionals: Vec<String>,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        output: ".".to_string(),
        to_stdout: false,
        force: false,
        verbose: false,
        quiet: false,
        level: 1,
        positionals: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-o" | "--output" => {
                i += 1;
                if i >= args.len() {
                    return Err("missing value for --output".to_string());
                }
                opts.output = args[i].clone();
            }
            "-f" | "--force" => opts.force = true,
            "-c" | "--stdout" => opts.to_stdout = true,
            "-v" | "--verbose" => opts.verbose = true,
            "-q" | "--quiet" => opts.quiet = true,
            arg if arg.starts_with('-') && arg.len() > 1 && arg[1..].chars().all(|c| c.is_ascii_digit()) => {
                if let Ok(lvl) = arg[1..].parse::<u8>() {
                    if lvl == 0 {
                        return Err("level must be >= 1".to_string());
                    }
                    opts.level = lvl;
                } else {
                    return Err(format!("invalid compression level: {arg}"));
                }
            }
            _ if a.starts_with('-') && a.len() > 1 => {
                return Err(format!("unknown option: {a}"));
            }
            _ => opts.positionals.push(a.clone()),
        }
        i += 1;
    }
    Ok(opts)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", bytes, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn cmd_create(opts: &Options) -> Result<(), String> {
    if opts.positionals.len() < 2 {
        return Err("usage: bounce create <archive.bnc> <files...>".to_string());
    }
    let archive = &opts.positionals[0];
    let inputs = &opts.positionals[1..];
    let stats =
        archive::create(archive, inputs, opts.level, opts.verbose, !opts.quiet).map_err(|e| format!("create: {e}"))?;
    if !opts.quiet {
        let ratio = if stats.orig_total == 0 {
            0.0
        } else {
            stats.stored_total as f64 / stats.orig_total as f64 * 100.0
        };
        println!(
            "{}: {} file(s), {} -> {} ({:.1}%)",
            archive,
            stats.files,
            human(stats.orig_total),
            human(stats.stored_total),
            ratio
        );
    }
    Ok(())
}

fn cmd_extract(opts: &Options) -> Result<(), String> {
    if opts.positionals.is_empty() {
        return Err("usage: bounce extract <archive.bnc> [files...]".to_string());
    }
    let archive = &opts.positionals[0];
    let filter = &opts.positionals[1..];
    if opts.to_stdout {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        archive::extract_to_writer(archive, &mut lock, filter)
            .map_err(|e| format!("extract: {e}"))?;
        return Ok(());
    }
    let n = archive::extract(archive, &opts.output, filter, opts.force, opts.verbose)
        .map_err(|e| format!("extract: {e}"))?;
    if !opts.quiet {
        println!("extracted {} file(s) to {}", n, opts.output);
    }
    Ok(())
}

fn cmd_list(opts: &Options) -> Result<(), String> {
    if opts.positionals.is_empty() {
        return Err("usage: bounce list <archive.bnc>".to_string());
    }
    let archive = &opts.positionals[0];
    let entries = archive::list_entries(archive).map_err(|e| format!("list: {e}"))?;

    println!(
        "{:>12}  {:>12}  {:>6}  {:>10}  Name",
        "Original", "Stored", "Ratio", "Method"
    );
    println!("{}", "-".repeat(64));
    let mut orig_total = 0u64;
    let mut stored_total = 0u64;
    for e in &entries {
        let ratio = if e.orig_size == 0 {
            0.0
        } else {
            e.stored_size as f64 / e.orig_size as f64 * 100.0
        };
        let method = if e.stored_raw {
            "stored"
        } else {
            codec::CompressMethod::from_u8(e.method)
                .map(codec::method_name)
                .unwrap_or("?")
        };
        println!(
            "{:>12}  {:>12}  {:>5.1}%  {:>10}  {}",
            human(e.orig_size),
            human(e.stored_size),
            ratio,
            method,
            e.path
        );
        orig_total += e.orig_size;
        stored_total += e.stored_size;
    }
    println!("{}", "-".repeat(64));
    let total_ratio = if orig_total == 0 {
        0.0
    } else {
        stored_total as f64 / orig_total as f64 * 100.0
    };
    println!(
        "{:>12}  {:>12}  {:>5.1}%  {:>10}  {} file(s)",
        human(orig_total),
        human(stored_total),
        total_ratio,
        "",
        entries.len()
    );
    Ok(())
}

fn cmd_test(opts: &Options) -> Result<(), String> {
    if opts.positionals.is_empty() {
        return Err("usage: bounce test <archive.bnc>".to_string());
    }
    let archive = &opts.positionals[0];
    let n = archive::test(archive, opts.verbose).map_err(|e| format!("test: {e}"))?;
    if !opts.quiet {
        println!("{}: OK ({} file(s) verified)", archive, n);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::FAILURE;
    }

    let command = args[0].clone();
    let rest = &args[1..];

    match command.as_str() {
        "h" | "help" | "-h" | "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        "v" | "version" | "-V" | "--version" => {
            println!("bounce {VERSION}");
            ExitCode::SUCCESS
        }
        "c" | "create" | "x" | "extract" | "l" | "list" | "t" | "test" => {
            let opts = match parse_options(rest) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let result = match command.as_str() {
                "c" | "create" => cmd_create(&opts),
                "x" | "extract" => cmd_extract(&opts),
                "l" | "list" => cmd_list(&opts),
                "t" | "test" => cmd_test(&opts),
                _ => unreachable!(),
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!("error: unknown command '{other}'\n");
            print_usage();
            ExitCode::FAILURE
        }
    }
}
