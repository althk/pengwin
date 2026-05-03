use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ext4-win", about = "Mount ext4 filesystems on Windows (read-only)")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Mount an ext4 filesystem
    Mount {
        /// Source: disk image file (.img/.raw) or raw disk (\\.\PhysicalDriveN)
        source: String,

        /// Drive letter to mount at (e.g. Z: or Z)
        drive: String,

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
