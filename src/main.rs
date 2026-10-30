mod agent;
mod config;
mod llm;
mod session;
mod tools;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use config::{CliOverrides, Config};
use agent::{Agent, AgentEvent};
use session::{Session, Message, Store};
use std::io::{self, IsTerminal, Write, Read};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name="openrust", version, about="A coding agent CLI + TUI")]
struct Cli {
    #[arg(short = 'p', long = "print")]
    print: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    #[arg(long)]
    workdir: Option<std::path::PathBuf>,
    #[arg(long)]
    yolo: bool,
    #[arg(long)]
    continue_session: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let overrides = CliOverrides { 
        provider: cli.provider, 
        model: cli.model, 
        base_url: cli.base_url, 
        config_path: cli.config, 
        yolo: cli.yolo, 
        workdir: cli.workdir 
    };
    let config = Config::load(overrides).context("loading configuration")?;

    if cli.print.is_some() || !io::stdin().is_terminal() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut agent = Agent::new(&config).map_err(|e| anyhow::anyhow!(e)).context("initializing agent")?;
        let mut session = Session::new();

        let user_msg = if let Some(p) = cli.print {
            p
        } else {
            let mut buffer = String::new();
            io::stdin().read_to_string(&mut buffer).context("reading stdin")?;
            buffer
        };
        
        session.push(Message::user(user_msg));
        
        let messages = session.messages.clone();
        
        tokio::spawn(async move {
            if let Err(e) = agent.run_turn(messages, &tx).await {
                let _ = tx.send(AgentEvent::Error(e));
            }
        });
        
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::Text(text) => {
                    print!("{}", text);
                    io::stdout().flush().ok();
                }
                AgentEvent::Message(msg) => {
                    session.push(msg);
                }
                AgentEvent::Error(e) => {
                    eprintln!("\nerror: {}", e);
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        
        let store = Store::new(config.workdir.join(".sessions")).context("opening store")?;
        store.save(&session).context("saving session")?;
        println!();
        return Ok(());
    }

    tui::run(config)
}
