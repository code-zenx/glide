//! The command line: what `glide` accepts, and where it finds its config.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{config_dir, Role};

#[derive(Parser)]
#[command(name = "glide", about = "Share one keyboard and mouse across machines")]
pub(super) struct Cli {
    /// Config file (default: ~/.config/glide/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    pub(super) cmd: Option<Cmd>,
}

/// What a bare `glide` does: the menu bar app on macOS, the daemon elsewhere.
pub(super) fn default_command() -> Cmd {
    #[cfg(target_os = "macos")]
    {
        Cmd::App
    }
    #[cfg(not(target_os = "macos"))]
    {
        Cmd::Run
    }
}

#[derive(Subcommand)]
pub(super) enum Cmd {
    /// Create the config and this machine's certificate
    Init {
        /// Name of this machine, as peers refer to it
        #[arg(long)]
        name: String,
        /// `server` if this machine's keyboard and mouse drive the other one,
        /// `client` if it is the one being driven
        #[arg(long)]
        role: Role,
    },
    /// Print this machine's certificate fingerprint
    Fingerprint,
    /// Connect to the configured peers and report latency
    Run,
    /// Print what the daemon is doing, for a status bar or a quick look
    Status {
        /// One JSON object per line, the shape waybar expects
        #[arg(long)]
        json: bool,
        /// Keep printing, within a tenth of a second of each change
        #[arg(long)]
        watch: bool,
        /// Print the app's letter instead of words, for a bar that draws it as
        /// a badge in CSS
        #[arg(long)]
        icon: bool,
    },
    /// Make a one-time invite (server): prints the `glide join …` command to
    /// run on the new machine. Valid for ten minutes.
    Invite,
    /// Pair this machine with a server, using the command its invite printed
    Join {
        token: String,
        /// This machine's name on the server (default: the hostname)
        #[arg(long)]
        name: Option<String>,
    },
    /// Steer the running daemon: pause, resume, release, peer on|off|forget
    /// <name>, extend <name>|off, view <name>|off, invite
    Ctl {
        #[arg(required = true, trailing_var_arg = true)]
        words: Vec<String>,
    },
    /// Run the daemon with a menu bar item: what the app bundle launches
    #[cfg(target_os = "macos")]
    App,
    /// Draw the arrange window to a PNG, for looking at the design
    #[cfg(target_os = "macos")]
    Snapshot {
        out: PathBuf,
        /// Pretend the peer sits on this side
        #[arg(long, default_value = "right")]
        side: String,
    },
}

pub(super) fn config_path(cli: &Cli) -> PathBuf {
    cli.config.clone().unwrap_or_else(|| config_dir().join("config.toml"))
}
