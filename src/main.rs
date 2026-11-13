use anyhow::{Context, Result, bail};
use clap::Parser;
use scribe::{
    cluster, home, input,
    model::Filter,
    output::{self, Format},
    timeutil, tui,
};
use std::io::{IsTerminal, Write};

/// Who changed the field, and when. Decodes Kubernetes managedFields.
///
/// With no input, scribe discovers your cluster (--kubeconfig, $KUBECONFIG,
/// ~/.kube/config, in-cluster) and opens a cluster overview. With files or piped
/// YAML/JSON (`kubectl get <obj> -o yaml --show-managed-fields | scribe`) it works
/// fully offline. On a terminal it is interactive; otherwise it prints to stdout.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// YAML/JSON files; `-` or none reads stdin. Multiple objects enable owner-chain pivoting.
    files: Vec<String>,

    /// Print instead of opening the TUI (implied when stdout is not a terminal).
    #[arg(long)]
    no_tui: bool,

    /// Output format for non-interactive mode.
    #[arg(short = 'o', long, value_enum, default_value = "table")]
    format: Format,

    /// Only fields whose path contains this text (case-insensitive) or matches a `*` glob.
    #[arg(short, long)]
    field: Option<String>,

    /// Only fields written by a manager containing this text (case-insensitive).
    #[arg(short, long)]
    manager: Option<String>,

    /// Kubeconfig file to use instead of $KUBECONFIG / ~/.kube/config.
    #[arg(long, value_name = "FILE")]
    kubeconfig: Option<std::path::PathBuf>,

    /// Kubeconfig context to use instead of the current one.
    #[arg(long, value_name = "NAME")]
    context: Option<String>,

    /// Only fields last written within this window, e.g. 30m, 2h, 7d.
    #[arg(long, value_name = "DURATION")]
    since: Option<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("scribe: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let now = timeutil::now_secs();
    let since_epoch = match &cli.since {
        Some(d) => Some(now - timeutil::parse_duration(d).with_context(|| format!("invalid --since {d:?} (try 30m, 2h, 7d)"))?),
        None => None,
    };
    let filter = Filter { field: cli.field, manager: cli.manager, since_epoch };

    // Input: files, or piped stdin. Nothing (a terminal, or an empty pipe such as
    // </dev/null in cron/CI) means "no input": discover a cluster instead.
    let objs = if cli.files.is_empty() {
        let mut text = String::new();
        if !std::io::stdin().is_terminal() {
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).context("reading stdin")?;
        }
        if text.trim().is_empty() {
            return cluster_mode(cli.kubeconfig, cli.context, cli.no_tui, now);
        }
        input::parse_documents(&text).context("in <stdin>")?
    } else {
        input::load(&cli.files)?
    };
    if objs.is_empty() {
        bail!("no objects found in input");
    }
    if objs.iter().all(|o| o.entry_count == 0) {
        bail!("no managedFields in input; kubectl hides them by default, re-run with `kubectl get ... -o yaml --show-managed-fields`");
    }
    let missing = objs.iter().filter(|o| o.entry_count == 0).count();
    if missing > 0 {
        eprintln!("scribe: warning: {missing} of {} object(s) have no managedFields (use --show-managed-fields)", objs.len());
    }

    if !cli.no_tui && std::io::stdout().is_terminal() {
        return tui::run(objs, filter);
    }
    let text = output::render(&objs, &filter, cli.format, now);
    // Ignore broken pipes (`scribe | head`).
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    Ok(())
}

/// No input given: discover a cluster and show the overview (TUI or text).
fn cluster_mode(kubeconfig: Option<std::path::PathBuf>, context: Option<String>, no_tui: bool, now: i64) -> Result<()> {
    let discovery = cluster::discover(&cluster::Env::from_process(), &cluster::Overrides { kubeconfig, context });
    let kubectl = cluster::find_in_path("kubectl");

    if !no_tui && std::io::stdout().is_terminal() {
        return home::run(home::Home::new(discovery, kubectl, now));
    }
    // Non-interactive: print what we know; fetch synchronously if we can.
    let fetched = match (&discovery.target, &kubectl) {
        (Some(t), Some(bin)) => Some(cluster::fetch(bin, t)),
        _ => None,
    };
    let text = home::summary(&discovery, kubectl.is_some(), fetched.as_ref());
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    if discovery.target.is_none() || kubectl.is_none() || matches!(fetched, Some(Err(_))) {
        std::process::exit(2);
    }
    Ok(())
}
