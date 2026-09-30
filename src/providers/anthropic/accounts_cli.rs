//! CLI entry point for proxy-owned Claude account registrations.

use anyhow::Result;
use clap::Subcommand;

use super::accounts;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage Claude accounts used to replace the forwarded login
    Accounts {
        #[command(subcommand)]
        command: AccountsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccountsCommand {
    /// Register a token from `claude setup-token` read from standard input
    Add { name: String },
    /// List registered accounts in switching order
    List,
    /// Remove a registered account
    Remove { name: String },
}

impl Command {
    pub fn run(self) -> Result<()> {
        match self {
            Self::Accounts { command } => run_accounts(command),
        }
    }
}

fn run_accounts(command: AccountsCommand) -> Result<()> {
    let store = accounts::file_store();
    match command {
        AccountsCommand::Add { name } => {
            eprintln!("Paste the token from `claude setup-token`:");
            let mut token = String::new();
            std::io::stdin().read_line(&mut token)?;
            match store.add(&name, &token) {
                Ok(account_count) => {
                    println!(
                        "Registered account {name} ({account_count} accounts) in {}",
                        store.path()
                    );
                    Ok(())
                }
                Err(err) => {
                    eprintln!("{err}");
                    std::process::exit(2);
                }
            }
        }
        AccountsCommand::List => {
            let stored = store.load()?;
            if stored.accounts.is_empty() {
                println!("No Claude accounts registered");
                return Ok(());
            }
            for (index, account) in stored.accounts.iter().enumerate() {
                println!(
                    "{}. {}  added {}",
                    index + 1,
                    account.name,
                    format_account_added_date(account.added_at)
                );
            }
            Ok(())
        }
        AccountsCommand::Remove { name } => {
            if let Err(err) = store.remove(&name) {
                eprintln!("{err}");
                std::process::exit(2);
            }
            println!("Removed account {name}");
            Ok(())
        }
    }
}

fn format_account_added_date(added_at_unix_milliseconds: u64) -> String {
    let added_at_unix_seconds = (added_at_unix_milliseconds / 1000) as i64;
    let Ok(added_at) = time::OffsetDateTime::from_unix_timestamp(added_at_unix_seconds) else {
        return added_at_unix_milliseconds.to_string();
    };
    let Ok(date_format) = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
    else {
        return added_at_unix_milliseconds.to_string();
    };
    added_at
        .format(&date_format)
        .unwrap_or_else(|_| added_at_unix_milliseconds.to_string())
}
