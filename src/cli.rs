//! Command line: `mcp`, `login`, `replay`, `script`, `report`, `tools`.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::config::Config;
use crate::ctx::{Ctx, Writes};
use crate::journal::Journal;
use crate::mcp::{self, OpDef};
use crate::util::stamp;
use crate::{browser, replay, report, script, view};

pub type BoxError = Box<dyn Error + Send + Sync>;

const USAGE: &str = "rustglance: browser tool for AI agents (MCP over stdio)

usage:
  rustglance mcp [--config FILE]       serve MCP on stdin and stdout
  rustglance login [--config FILE]     open a window so a human can log in
  rustglance replay SCRIPT [--writes] [--param NAME=VALUE]... [--config FILE]
                                       run a script or journal without a model
  rustglance script RUN1 [RUN2]        print a script from journals; with two runs
                                       of one task, differing values become {{pN}}
  rustglance report JOURNAL            print a markdown summary of a journal
  rustglance tools                     print the tool list as JSON

config: --config FILE, else $RUSTGLANCE_CONFIG, else the defaults in the README";

struct Cli {
    cmd: String,
    files: Vec<PathBuf>,
    config: Option<PathBuf>,
    writes: bool,
    params: Vec<(String, String)>,
}

fn parse(args: Vec<String>) -> Result<Cli, String> {
    let mut cli = Cli {
        cmd: String::new(),
        files: Vec::new(),
        config: None,
        writes: false,
        params: Vec::new(),
    };
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => cli.config = Some(it.next().ok_or("--config needs a file")?.into()),
            "--writes" => cli.writes = true,
            "--param" => {
                let p = it.next().ok_or("--param needs NAME=VALUE")?;
                let (k, v) = p.split_once('=').ok_or(format!("--param {p:?} is not NAME=VALUE"))?;
                cli.params.push((k.to_string(), v.to_string()));
            }
            "-h" | "--help" => {
                return Ok(Cli {
                    cmd: "help".to_string(),
                    ..cli
                });
            }
            "-V" | "--version" => {
                return Ok(Cli {
                    cmd: "version".to_string(),
                    ..cli
                });
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ if cli.cmd.is_empty() => cli.cmd = a,
            _ => cli.files.push(a.into()),
        }
    }
    if cli.cmd != "replay" && (cli.writes || !cli.params.is_empty()) {
        return Err("--writes and --param belong to replay".to_string());
    }
    Ok(cli)
}

/// Entry point for the binary and for extensions that add tools.
pub fn main_with(ops: Vec<OpDef>) -> ExitCode {
    let args: Result<Vec<String>, String> = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().map_err(|a| format!("argument {a:?} is not UTF-8")))
        .collect();
    let cli = match args.and_then(parse) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rustglance: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("rustglance: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(run(cli, ops)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rustglance: {e}");
            let mut cause = e.source();
            while let Some(c) = cause {
                eprintln!("  because: {c}");
                cause = c.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn only_one(cli: &Cli, what: &str) -> Result<PathBuf, BoxError> {
    match cli.files.as_slice() {
        [f] => Ok(f.clone()),
        _ => Err(format!("{} needs exactly one {what}", cli.cmd).into()),
    }
}

async fn run(cli: Cli, ops: Vec<OpDef>) -> Result<ExitCode, BoxError> {
    match cli.cmd.as_str() {
        "" => {
            eprintln!("{USAGE}");
            Ok(ExitCode::from(2))
        }
        "help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        "version" => {
            println!("rustglance {}", env!("CARGO_PKG_VERSION"));
            Ok(ExitCode::SUCCESS)
        }
        "tools" => {
            println!("{}", serde_json::to_string_pretty(&mcp::tool_list(&ops))?);
            Ok(ExitCode::SUCCESS)
        }
        "mcp" => {
            if !cli.files.is_empty() {
                return Err("mcp takes no files".into());
            }
            let cfg = Config::load(cli.config.as_deref())?;
            let ctx = Ctx::start(cfg, Writes::Config).await?;
            let path = ctx.cfg.evidence.join(format!("journal-{}.jsonl", stamp()));
            let journal = Journal::open(&path).map_err(|e| format!("cannot open journal {}: {e}", path.display()))?;
            eprintln!("rustglance: journal {}", path.display());
            // The live view is for a human watching; the tools work without it.
            if let Some(port) = ctx.cfg.view_port {
                if let Err(e) = view::start(ctx.clone(), port).await {
                    eprintln!("rustglance: live view off: {e}");
                }
            }
            let served = mcp::serve(&ctx, &ops, &journal).await;
            ctx.shutdown().await;
            served?;
            Ok(ExitCode::SUCCESS)
        }
        "login" => {
            if !cli.files.is_empty() {
                return Err("login takes no files".into());
            }
            let cfg = Config::load(cli.config.as_deref())?;
            browser::login(&cfg).await?;
            Ok(ExitCode::SUCCESS)
        }
        "replay" => {
            let file = only_one(&cli, "script file")?;
            let cfg = Config::load(cli.config.as_deref())?;
            replay::run(cfg, &ops, &file, cli.writes, &cli.params).await
        }
        "script" => {
            if cli.files.is_empty() || cli.files.len() > 2 {
                return Err("script needs one or two journal files".into());
            }
            let built = script::build(&cli.files)?;
            for note in &built.notes {
                eprintln!("{note}");
            }
            print!("{}", built.text);
            Ok(ExitCode::SUCCESS)
        }
        "report" => {
            let file = only_one(&cli, "journal file")?;
            print!("{}", report::render(&file)?);
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}").into()),
    }
}
