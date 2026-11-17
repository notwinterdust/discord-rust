//! A minimal bot using the default TLS transports.
//!
//! Run it with a bot token:
//!
//! ```text
//! DISCORD_TOKEN="..." cargo run --example ping_bot
//! ```
//!
//! The bot answers `!ping` and `/ping`, logs incoming messages, and prints the
//! account name once the gateway is ready.

use std::error::Error;

use discord_rust::{BotBuilder, CommandRegistry, Context, Event, Intents, SlashCommandRegistry};

fn main() -> Result<(), Box<dyn Error>> {
    let token = std::env::var("DISCORD_TOKEN")?;
    // register prefix
    let mut prefix = CommandRegistry::new("!");
    // add ping command
    prefix.add_command("ping", |context: &Context| context.respond("Pong!"))?;
    // register slash command
    let mut slash = SlashCommandRegistry::new();
    // add slash command
    slash.add(
        "ping",
        "Check that the bot is online",
        |context: &Context| context.respond("Pong!"),
    )?;
    // create the bot
    let bot = BotBuilder::new()
        .token(token)
        .intents_flags(Intents::default() | Intents::MESSAGE_CONTENT)
        .commands(prefix)
        .slash_commands(slash)
        .on_event_name("MESSAGE_CREATE", |event| {
            if let Event::MessageCreate(message) = event {
                println!("{}: {}", message.author.user().name, message.content);
            }
        })
        .on_event_name("READY", |event| {
            if let Event::Ready(ready) = event {
                println!("Connected as {}", ready.user.user.name);
            }
        })
        .build()?;

    println!("Starting bot; press Ctrl+C to stop.");
    // run_auto to use the default TLS transports
    if let Err(error) = bot.run_auto() {
        eprintln!("Bot stopped: {error}");
        std::process::exit(1);
    }
    Ok(())
}
