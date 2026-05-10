use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ext4-win", about = "Mount ext4 filesystems on Windows (read-only by default)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Mount an ext4 filesystem (read-only unless --rw is given)
    Mount {
        /// Source: disk image file (.img/.raw) or raw disk (\\.\PhysicalDriveN)
        source: String,

        /// Drive letter to mount at (e.g. Z: or Z)
        drive: String,

        /// Mount read-write. Without this flag, the mount is read-only and the
        /// device will not be modified.
        #[arg(long)]
        rw: bool,

        /// Allow read-only mount even if the journal is dirty. Skips replay;
        /// uncommitted journal contents will not be visible. Ignored with --rw.
        #[arg(long)]
        force: bool,

        /// Enable verbose debug logging
        #[arg(short, long)]
        verbose: bool,
    },

    /// Unmount a previously mounted ext4 filesystem
    Unmount {
        /// Drive letter to unmount
        drive: String,
    },
}
