// Target-specific transitive dependency split (mio/crossterm stack) is accepted for now.
#![allow(clippy::multiple_crate_versions)]

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand, ValueEnum};
use keron_engine::{
    ApplyOptions, ProviderRegistry, apply_operation_from_file, apply_plan, build_plan_for_folder,
    has_potentially_destructive_forced_changes,
};
use keron_report::{
    ColorChoice, OutputFormat, RenderOptions, redact_sensitive, render_apply, render_plan,
};
use keron_source::resolve_apply_source;
use minus::{ExitStrategy, Pager, page_all};

mod error;

pub use error::CliError;

#[derive(Debug, Parser)]
#[command(name = "keron", about = "Lua-based dotfile manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Apply {
        source: String,
        #[command(flatten)]
        render: RenderFlags,
        #[arg(long, value_enum, default_value_t = FormatArg::Text)]
        format: FormatArg,
        #[arg(long)]
        execute: bool,
    },
    #[command(name = "__apply-op", hide = true)]
    ApplyOperation {
        #[arg(long = "op-file")]
        op_file: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FormatArg {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColorArg {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Args)]
struct RenderFlags {
    #[arg(long, value_enum, default_value_t = ColorArg::Auto)]
    color: ColorArg,
    #[arg(long)]
    verbose: bool,
}

impl RenderFlags {
    fn render_options(&self, target: &str) -> RenderOptions {
        RenderOptions {
            color: self.color.into(),
            verbose: self.verbose,
            target: Some(target.to_string()),
        }
    }
}

impl From<FormatArg> for OutputFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Text => Self::Text,
            FormatArg::Json => Self::Json,
        }
    }
}

impl From<ColorArg> for ColorChoice {
    fn from(value: ColorArg) -> Self {
        match value {
            ColorArg::Auto => Self::Auto,
            ColorArg::Always => Self::Always,
            ColorArg::Never => Self::Never,
        }
    }
}

/// Run the CLI using process arguments.
///
/// # Errors
///
/// Returns an error when argument parsing fails (excluding help/version) or command
/// execution fails.
pub fn run() -> std::result::Result<i32, CliError> {
    run_from(std::env::args_os())
}

fn run_from<I, T>(args: I) -> std::result::Result<i32, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(error) => match error.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                print!("{error}");
                return Ok(0);
            }
            _ => return Err(error.into()),
        },
    };
    let providers = ProviderRegistry::builtin();

    match cli.command {
        Commands::Apply {
            source,
            render,
            format,
            execute,
        } => {
            let resolved = resolve_apply_source(&source)?;
            let (report, sensitive_values) =
                build_plan_for_folder(&resolved.manifest_root, &providers)?;
            let render_options = render.render_options(&resolved.display_target);
            let output_format: OutputFormat = format.into();
            if report.has_errors() {
                let rendered = render_plan(&report, output_format, &render_options)?;
                emit_output(&rendered, output_format, &sensitive_values);
                return Ok(1);
            }

            if !execute {
                let has_drift = report.has_drift();
                let rendered = render_plan(&report, output_format, &render_options)?;
                emit_output(&rendered, output_format, &sensitive_values);
                if has_drift && output_format == OutputFormat::Text {
                    eprintln!("hint: re-run with --execute to apply changes");
                }
                return Ok(if has_drift { 2 } else { 0 });
            }

            if output_format == OutputFormat::Text
                && render_options.verbose
                && has_potentially_destructive_forced_changes(&report)
            {
                eprintln!(
                    "warning: plan includes force=true replacements that may overwrite/remove existing paths"
                );
            }

            let (apply_report, apply_sensitive) =
                apply_plan(&report, &providers, ApplyOptions::default());
            let mut all_sensitive = sensitive_values;
            all_sensitive.extend(apply_sensitive);
            let rendered = render_apply(&apply_report, output_format, &render_options)?;
            emit_output(&rendered, output_format, &all_sensitive);
            Ok(i32::from(apply_report.has_failures()))
        }
        Commands::ApplyOperation { op_file } => {
            let _ = apply_operation_from_file(&op_file, &providers)?;
            Ok(0)
        }
    }
}

fn emit_output(rendered: &str, format: OutputFormat, sensitive_values: &BTreeSet<String>) {
    let redacted = redact_sensitive(rendered, sensitive_values);

    if format == OutputFormat::Text && should_use_pager() && page_output(&redacted).is_ok() {
        return;
    }

    if redacted.ends_with('\n') {
        print!("{redacted}");
    } else {
        println!("{redacted}");
    }
}

fn should_use_pager() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_PAGER").is_none()
}

fn page_output(rendered: &str) -> std::result::Result<(), minus::MinusError> {
    let pager = Pager::new();
    pager.set_exit_strategy(ExitStrategy::PagerQuit)?;
    pager.set_text(rendered)?;
    page_all(pager)
}
