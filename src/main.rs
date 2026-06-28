mod service;
mod stats;
mod telegram_message;

use std::{sync::Arc, time::Duration};
use telegram_message::{InlineCode, TelegramMessage};
use thiserror::Error;
use anyhow::{Error, Result};
use clap::Parser;
use poise::{
    serenity_prelude::{
        GatewayIntents,
        ClientBuilder,
        CreateEmbed,
        CreateEmbedFooter,
        User,
        Colour
    },
    CreateReply
};
use service::{
    UserApplication,
    WalletService,
    WalletServiceImpl
};
use stats::StatsConfig;
use teloxide::{
    dispatching::{HandlerExt, UpdateFilterExt},
    prelude::{dptree, Dispatcher, Requester},
    types::{Message, Update},
    utils::command::BotCommands,
    Bot
};
use xelis_common::{
    async_handler,
    crypto::Address,
    network::Network,
    prompt::{
        argument::{Arg, ArgType, ArgumentManager},
        command::{
            Command,
            CommandError,
            CommandHandler,
            CommandManager
        },
        LogLevel,
        ModuleConfig,
        Prompt,
        PromptError,
        default_logs_datetime_format,
    },
    utils::{format_xelis, from_xelis, detect_available_parallelism}
};
use xelis_wallet::config::DEFAULT_DAEMON_ADDRESS;
use log::{debug, error};

// Context type for poise with our data type
type Context<'a> = poise::Context<'a, WalletService, Error>;

// Icon URL for thumbnail
const ICON: &str = "https://github.com/xelis-project/xelis-assets/raw/master/icons/png/square/green_background_black_logo.png?raw=true";
// Color of the embed
const COLOR: u32 = 196559;

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("No user found")]
    NoUser
}

#[derive(Parser)]
#[clap(version = "1.0.0", about = "XELIS Tip Bot")]
#[command(styles = xelis_common::get_cli_styles())]
pub struct Config {
    /// Network selected for wallet
    #[clap(long, value_enum, default_value_t = Network::Mainnet)]
    network: Network,
    /// Password for wallet
    #[clap(short, long)]
    password: String,
    /// Name for the wallet
    #[clap(short, long)]
    wallet_name: String,
    /// Daemon address for wallet
    #[clap(short, long, default_value_t = String::from(DEFAULT_DAEMON_ADDRESS))]
    daemon_address: String,
    /// Discord bot token
    #[clap(long)]
    discord_token: String,
    /// Telegram bot token
    #[clap(long)]
    telegram_token: String,
    /// Set log level
    #[clap(long, value_enum, default_value_t = LogLevel::Info)]
    log_level: LogLevel,
    /// Set file log level
    #[clap(long, value_enum, default_value_t = LogLevel::Info)]
    file_log_level: LogLevel,
    /// Disable the log file
    #[clap(long)]
    disable_file_logging: bool,
    /// Disable the log filename date based
    /// If disabled, the log file will be named xelis-tipbot.log instead of YYYY-MM-DD.xelis-tipbot.log
    #[clap(long)]
    disable_file_log_date_based: bool,
    /// Disable the usage of colors in log
    #[clap(long)]
    disable_log_color: bool,
    /// Enable the log file auto compression
    /// If enabled, the log file will be compressed every day
    /// This will only work if the log file is enabled
    #[clap(long)]
    auto_compress_logs: bool,
    /// Disable terminal interactive mode
    /// You will not be able to write CLI commands in it or to have an updated prompt
    #[clap(long)]
    disable_interactive_mode: bool,
    /// Log filename
    /// 
    /// By default filename is xelis-wallet.log.
    /// File will be stored in logs directory, this is only the filename, not the full path.
    /// Log file is rotated every day and has the format YYYY-MM-DD.xelis-tipbot.log.
    #[clap(long, default_value_t = String::from("xelis-tipbot.log"))]
    filename_log: String,
    /// Logs directory
    /// 
    /// By default it will be logs/ of the current directory.
    /// It must end with a / to be a valid folder.
    #[clap(long, default_value_t = String::from("logs/"))]
    logs_path: String,
    /// Module configuration for logs
    #[clap(long)]
    logs_modules: Vec<ModuleConfig>,
    /// Set the path for wallet storage to open/create a wallet at this location
    #[clap(long)]
    wallet_path: Option<String>,
    /// Disable the ascii art at startup
    #[clap(long)]
    disable_ascii_art: bool,
    /// Change the datetime format used by the logger
    #[clap(long, default_value_t = default_logs_datetime_format())]
    datetime_format: String,
    /// How many threads we want to use
    /// during ciphertext decryption
    #[clap(long, default_value_t = detect_available_parallelism())]
    pub n_decryption_threads: usize,
    /// Concurrency configuration for Network Handler
    #[clap(long, default_value_t = detect_available_parallelism())]
    pub network_concurrency: usize,
    /// XELIS stats channel updater configuration
    #[clap(flatten)]
    stats: StatsConfig,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "These commands are supported:")]
pub enum TelegramCommand {
    #[command(description = "start the bot.")]
    Start,
    #[command(description = "display this text.")]
    Help,
    #[command(description = "display the status of the wallet.")]
    Status,
    #[command(description = "display your balance.")]
    Balance,
    #[command(description = "display your deposit address.")]
    Deposit,
    #[command(description = "withdraw from your balance.", parse_with = "split")]
    Withdraw { address: String, amount: f64 },
    #[command(description = "tip the user to which you reply")]
    Tip { amount: f64 },
}

impl TelegramCommand {
    pub fn allow_public(&self) -> bool {
        match self {
            TelegramCommand::Tip { amount: _ } => true,
            _ => false
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    xelis_common::config::init();

    let mut config = Config::parse();

    // Init wallet service
    let service = WalletServiceImpl::new(
        &config.wallet_name,
        &config.password,
        config.daemon_address,
        config.network,
        config.n_decryption_threads,
        config.network_concurrency
    ).await?;

    // Init discord bot
    let mut discord_client = {
        let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;
    
        // Create the framework
        let framework = {
            let service = service.clone();
            poise::Framework::builder()
                .options(poise::FrameworkOptions {
                    commands: vec![status(), balance(), deposit(), withdraw(), tip()],
                    ..Default::default()
                })
                .setup(|ctx, _ready, framework| {
                    Box::pin(async move {
                        poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                        Ok(service)
                    })
                })
                .build()
        };
    
        // Create the client using token and intents
        ClientBuilder::new(config.discord_token, intents)
            .framework(framework)
            .await?
    };

    // Telegram bot
    let (telegram_client, bot) = {
        let bot = Bot::new(config.telegram_token);
        let instance = bot.clone();
        let service = service.clone();
        let handle = tokio::spawn(async move {
            let handler = Update::filter_message()
                .branch(
                    dptree::entry()
                        .filter_command::<TelegramCommand>()
                        .endpoint(telegram_handler)
                );
    
            Dispatcher::builder(bot, handler)
                .dependencies(dptree::deps![service])
                .enable_ctrlc_handler()
                .build()
                .dispatch().await
        });

        (handle, instance)
    };

    // start the service
    Arc::clone(&service).start(discord_client.http.clone(), bot).await?;

    if config.stats.enabled() {
        stats::spawn_stats_updater(discord_client.http.clone(), service.clone(), config.stats.clone());
    }

    config.logs_modules.push(ModuleConfig { module: "serenity".to_string(), level: LogLevel::Warn });
    let prompt = Prompt::new(
        config.log_level,
        &config.logs_path,
        &config.filename_log,
        config.disable_file_logging,
        config.disable_file_log_date_based,
        config.disable_log_color,
        config.auto_compress_logs,
        !config.disable_interactive_mode,
        config.logs_modules,
        config.file_log_level,
        true,
        config.datetime_format,
    )?;

    let command_manager = CommandManager::new(prompt.clone());
    command_manager.store_in_context(service)?;

    command_manager.register_default_commands()?;
    command_manager.add_command(Command::new("rescan", "Rescan the wallet", CommandHandler::Async(async_handler!(rescan))))?;
    command_manager.add_command(Command::new("clear_balances", "Clear all balances", CommandHandler::Async(async_handler!(clear_balances))))?;
    command_manager.add_command(Command::with_required_arguments("add_balance_discord", "Add balance to a discord user", vec![Arg::new("user_id", ArgType::Number), Arg::new("amount", ArgType::String)], CommandHandler::Async(async_handler!(add_balance_discord))))?;
    command_manager.add_command(Command::with_required_arguments("remove_balance_discord", "Remove balance from a discord user", vec![Arg::new("user_id", ArgType::Number), Arg::new("amount", ArgType::String)], CommandHandler::Async(async_handler!(remove_balance_discord))))?;

    command_manager.add_command(Command::with_required_arguments("withdraw", "Withdraw an amount to an address", vec![Arg::new("address", ArgType::String), Arg::new("amount", ArgType::String)], CommandHandler::Async(async_handler!(withdraw_cmd))))?;
    command_manager.add_command(Command::with_required_arguments("withdraw_all", "Withdraw the whole balance to an address", vec![Arg::new("address", ArgType::String)], CommandHandler::Async(async_handler!(withdraw_all))))?;

    command_manager.display_commands()?;

    tokio::select! {
        // start listening for events by starting a single shard
        res = discord_client.start() => {
            if let Err(e) = res {
                error!("An error occurred while running the client: {:?}", e);
            }
        },
        _ = telegram_client => {
            error!("Telegram client stopped");
        },
        res = prompt.start(Duration::from_millis(1000), Box::new(async_handler!(prompt_message_builder)), Some(&command_manager)) => {
            if let Err(e) = res {
                error!("An error occurred while running the prompt: {:?}", e);
            }
        }
    };

    Ok(())
}

// Default prompt message builder
async fn prompt_message_builder(_: &Prompt, _: Option<&CommandManager>) -> Result<String, PromptError> {
    Ok("XELIS Tip Bot >>".to_string())
}

// Rescan CLI command
async fn rescan(manager: &CommandManager, _: ArgumentManager) -> Result<(), CommandError> {
    let context = manager.get_context().lock()?;
    let service: &WalletService = context.get()?;
    if let Err(e) = service.rescan().await {
        manager.error(format!("An error occurred while rescanning the wallet: {}", e.to_string()));
    } else {
        manager.message("Wallet has been rescanned");
    }

    Ok(())
}

// Clear all balances
async fn clear_balances(manager: &CommandManager, _: ArgumentManager) -> Result<(), CommandError> {
    let context = manager.get_context().lock()?;
    let service: &WalletService = context.get()?;
    if let Err(e) = service.clear_balances().await {
        manager.error(format!("An error occurred while clearing the balances: {}", e.to_string()));
    } else {
        manager.message("Balances have been cleared");
    }

    Ok(())
}

// Add balance to discord user
async fn add_balance_discord(manager: &CommandManager, mut args: ArgumentManager) -> Result<(), CommandError> {
    let context = manager.get_context().lock()?;
    let service: &WalletService = context.get()?;
    let user_id = args.get_value("user_id")?.to_number()?;
    let amount = args.get_value("amount")?.to_string_value()?;
    let amount = from_xelis(amount).ok_or_else(|| CommandError::InvalidArgument("amount".to_string()))?;

    if let Err(e) = service.add_balance(&UserApplication::Discord(user_id.into()), amount).await {
        manager.error(format!("An error occurred while adding balance: {}", e.to_string()));
    } else {
        manager.message("Balance has been added");
    }

    Ok(())
}

async fn remove_balance_discord(manager: &CommandManager, mut args: ArgumentManager) -> Result<(), CommandError> {
    let context = manager.get_context().lock()?;
    let service: &WalletService = context.get()?;
    let user_id = args.get_value("user_id")?.to_number()?;
    let amount = args.get_value("amount")?.to_string_value()?;
    let amount = from_xelis(amount).ok_or_else(|| CommandError::InvalidArgument("amount".to_string()))?;

    if let Err(e) = service.remove_balance(&UserApplication::Discord(user_id.into()), amount).await {
        manager.error(format!("An error occurred while removing balance: {}", e.to_string()));
    } else {
        manager.message("Balance has been removed");
    }

    Ok(())
}

// Withdraw all balance
async fn withdraw_all(manager: &CommandManager, mut args: ArgumentManager) -> Result<(), CommandError> {
    let context = manager.get_context().lock()?;
    let service: &WalletService = context.get()?;
    let address = args.get_value("address")?.to_string_value()?;

    let address = Address::from_string(&address).map_err(|e| CommandError::InvalidArgument(e.to_string()))?;

    if let Err(e) = service.withdraw_all(address).await {
        manager.error(format!("An error occurred while withdrawing all: {}", e.to_string()));
    } else {
        manager.message("All balance has been withdrawn");
    }

    Ok(())
}

// Withdraw all balance
async fn withdraw_cmd(manager: &CommandManager, mut args: ArgumentManager) -> Result<(), CommandError> {
    let context = manager.get_context().lock()?;
    let service: &WalletService = context.get()?;
    let address = args.get_value("address")?.to_string_value()?;

    let amount = args.get_value("amount")?.to_string_value()?;
    let amount = from_xelis(amount).ok_or_else(|| CommandError::InvalidArgument("amount".to_string()))?;

    let address = Address::from_string(&address).map_err(|e| CommandError::InvalidArgument(e.to_string()))?;

    if let Err(e) = service.withdraw_to(address, amount).await {
        manager.error(format!("An error occurred while withdrawing all: {}", e.to_string()));
    } else {
        manager.message("Amount has been withdrawn");
    }

    Ok(())
}


/// See the status of the wallet
#[poise::command(slash_command, broadcast_typing)]
async fn status(ctx: Context<'_>) -> Result<(), Error> {
    // Retrieve balance for user
    let service = ctx.data();
    let balance = service.get_wallet_balance().await?;
    let total_balance = service.get_total_users_balance().await?;
    let topoheight = service.get_wallet_topoheight().await?;
    let network = service.network();
    let online = service.is_wallet_online().await;

    let embed = CreateEmbed::default()
        .title("Status")
        .field("Wallet Balance", format_xelis(balance), false)
        .field("Total Users Balance", format_xelis(total_balance), false)
        .field("Synced TopoHeight", topoheight.to_string(), false)
        .field("Network", network.to_string(), false)
        .field("Is Online", online.to_string(), false)
        .thumbnail(ICON)
        .colour(COLOR);
    let mut reply = CreateReply::default()
        .embed(embed);

    // Set reply to ephemeral if command was not used in DM
    if ctx.channel_id().to_channel(ctx.http()).await?.private().is_none() {
        reply = reply.ephemeral(true);
    }

    // Send reply
    ctx.send(reply).await?;

    Ok(())
}

/// Show your current balance
#[poise::command(slash_command, broadcast_typing)]
async fn balance(ctx: Context<'_>) -> Result<(), Error> {
    // Retrieve balance for user
    let service = ctx.data();
    let balance = service.get_balance_for_user(&UserApplication::Discord(ctx.author().id.into())).await;

    let embed = CreateEmbed::default()
        .title("Balance")
        .field("Your balance is", format_xelis(balance), false)
        .thumbnail(ICON)
        .colour(COLOR);
    let mut reply = CreateReply::default()
        .embed(embed);

    // Set reply to ephemeral if command was not used in DM
    if ctx.channel_id().to_channel(ctx.http()).await?.private().is_none() {
        reply = reply.ephemeral(true);
    }

    // Send reply
    ctx.send(reply).await?;

    Ok(())
}

/// Show your deposit address
#[poise::command(slash_command, broadcast_typing)]
async fn deposit(ctx: Context<'_>) -> Result<(), Error> {
    // Retrieve address for user
    let service = ctx.data();
    let address = service.get_address_for_user(&UserApplication::Discord(ctx.author().id.into()));

    let embed = CreateEmbed::default()
        .title("Deposit")
        .field("Your deposit address is", address.to_string(), true)
        .footer(CreateEmbedFooter::new("Please do not send any other coins than XELIS to this address"))
        .thumbnail(ICON)
        .colour(COLOR);

    let mut reply = CreateReply::default()
        .embed(embed);

    // Set reply to ephemeral if command was not used in DM
    if ctx.channel_id().to_channel(ctx.http()).await?.private().is_none() {
        reply = reply.ephemeral(true);
    }

    // Send reply
    ctx.send(reply).await?;

    Ok(())
}

/// Withdraw from your balance
#[poise::command(slash_command, broadcast_typing)]
async fn withdraw(ctx: Context<'_>, address: String, amount: f64) -> Result<(), Error> {
    let service = ctx.data();
    let ephemeral = ctx.channel_id().to_channel(ctx.http()).await?.private().is_none();

    // Parse address in correct format
    let to = match Address::from_string(&address) {
        Ok(address) => address,
        Err(e) => {
            ctx.send(CreateReply::default().ephemeral(ephemeral).embed(
                CreateEmbed::default()
                    .title("Withdraw")
                    .field("An error occured while withdrawing", e.to_string(), false)
                    .thumbnail(ICON)
                    .colour(Colour::RED)
                )
            ).await?;
            return Ok(());
        }    
    };

    // Verify the address is in good network
    if to.is_mainnet() != service.network().is_mainnet() {
        ctx.send(CreateReply::default().ephemeral(ephemeral).embed(
            CreateEmbed::default()
                .title("Withdraw")
                .field("An error occured while withdrawing", "Invalid network", false)
                .thumbnail(ICON)
                .colour(Colour::RED)
            )
        ).await?;
        return Ok(());
    }

    // Parse amount in correct format
    let amount = match from_xelis(amount.to_string()) {
        Some(amount) => amount,
        None => {
            ctx.send(CreateReply::default().ephemeral(true).embed(
                CreateEmbed::default()
                    .title("Tip")
                    .field("An error occured while tipping", "Invalid amount", false)
                    .thumbnail(ICON)
                    .colour(Colour::RED)
                )
            ).await?;
            return Ok(());
        }
    };

    match service.withdraw(&UserApplication::Discord(ctx.author().id.into()), to, amount).await {
        Ok(hash) => {
            ctx.send(CreateReply::default().ephemeral(ephemeral).embed(
                CreateEmbed::default()
                    .title("Withdraw")
                    .description(format!("You have withdrawn {} XEL", format_xelis(amount)))
                    .field("Transaction", hash.to_string(), false)
                    .thumbnail(ICON)
                    .colour(COLOR)
                )
            ).await?;
        },
        Err(e) => {
            ctx.send(CreateReply::default().ephemeral(ephemeral).embed(
                CreateEmbed::default()
                    .title("Withdraw")
                    .field("An error occured while withdrawing", e.to_string(), false)
                    .thumbnail(ICON)
                    .colour(Colour::RED)
                )
            ).await?;
        }
    };

    Ok(())
}

/// Tip a user with XELIS
#[poise::command(slash_command, broadcast_typing)]
async fn tip(ctx: Context<'_>, #[description = "User to tip"] user: User, #[description = "Amount to tip"] amount: f64) -> Result<(), Error> {
    let amount = match from_xelis(amount.to_string()) {
        Some(amount) => amount,
        None => {
            ctx.send(CreateReply::default().ephemeral(true).embed(
                CreateEmbed::default()
                    .title("Tip")
                    .field("An error occured while tipping", "Invalid amount", false)
                    .thumbnail(ICON)
                    .colour(Colour::RED)
                )
            ).await?;
            return Ok(());
        }
    };

    // Retrieve address for user
    let service = ctx.data();

    match service.transfer(&UserApplication::Discord(ctx.author().id.into()), &UserApplication::Discord(user.id.into()), amount).await {
        Ok(_) => {
            ctx.send(CreateReply::default().embed(
                CreateEmbed::default()
                    .title("Tip")
                    .description(format!("{} have tipped {} XEL to {}", ctx.author(), format_xelis(amount), user))
                    .thumbnail(ICON)
                    .colour(COLOR)
                )
            ).await?;
        },
        Err(e) => {
            ctx.send(CreateReply::default().ephemeral(true).embed(
                CreateEmbed::default()
                    .title("Tip")
                    .field("An error occured while tipping", e.to_string(), false)
                    .thumbnail(ICON)
                    .colour(Colour::RED)
                )
            ).await?;
        }
    };

    Ok(())
}

// Handler for telegram bot
async fn telegram_handler(bot: Bot, msg: Message, cmd: TelegramCommand, state: WalletService) -> Result<(), Error> {
    if !cmd.allow_public() && !msg.chat.is_private() {
        let from = msg.from.ok_or(TelegramError::NoUser)?;
        bot.send_message(from.id, "You can only use this command in private").await?;
        return Ok(());
    }

    let thread_id = msg.thread_id.filter(|_| msg.is_topic_message);
    match cmd {
        TelegramCommand::Start => {
            TelegramMessage::new(&bot, msg.chat.id, thread_id)
                .title("Welcome")
                .field("Welcome to the XELIS Tip Bot!", "You can use /help to see the available commands", false)
                .send().await?;
        }
        TelegramCommand::Help => {
            bot.send_message(msg.chat.id, TelegramCommand::descriptions().to_string()).await?;
        },
        TelegramCommand::Status => {
            let balance = state.get_wallet_balance().await?;
            let total_balance = state.get_total_users_balance().await?;
            let topoheight = state.get_wallet_topoheight().await?;
            let network = state.network();
            let online = state.is_wallet_online().await;

            TelegramMessage::new(&bot, msg.chat.id, thread_id)
                .title("Status")
                .field("Wallet Balance", format_xelis(balance), false)
                .field("Total Users Balance", format_xelis(total_balance), false)
                .field("Synced TopoHeight", topoheight.to_string(), false)
                .field("Network", network.to_string(), false)
                .field("Is Online", online.to_string(), false)
                .send().await?;
        },
        TelegramCommand::Balance => {
            let from = msg.from.ok_or(TelegramError::NoUser)?;
            let balance = state.get_balance_for_user(&UserApplication::Telegram(from.id.0)).await;

            TelegramMessage::new(&bot, msg.chat.id, thread_id)
                .title("Balance")
                .field("Your balance is", format_xelis(balance), false)
                .send().await?;
        },
        TelegramCommand::Deposit => {
            let from = msg.from.ok_or(TelegramError::NoUser)?;
            let address = state.get_address_for_user(&UserApplication::Telegram(from.id.0));

            TelegramMessage::new(&bot, msg.chat.id, thread_id)
                .title("Deposit")
                .field("Your deposit address is", InlineCode::new(&address.to_string()), false)
                .field("Please do not send any other coins than XELIS to this address", "", false)
                .send().await?;
        },
        TelegramCommand::Withdraw { address, amount } => {
            let from = msg.from.ok_or(TelegramError::NoUser)?;
            let to = match Address::from_string(&address) {
                Ok(address) => address,
                Err(e) => {
                    bot.send_message(msg.chat.id, format!("An error occured while withdrawing: {}", e)).await?;
                    return Ok(());
                }    
            };

            if to.is_mainnet() != state.network().is_mainnet() {
                bot.send_message(msg.chat.id, "An error occured while withdrawing: Invalid network").await?;
                return Ok(());
            }

            let amount = match from_xelis(amount.to_string()) {
                Some(amount) => amount,
                None => {
                    bot.send_message(msg.chat.id, "An error occured while withdrawing: Invalid amount").await?;
                    return Ok(());
                }
            };

            match state.withdraw(&UserApplication::Telegram(from.id.0), to, amount).await {
                Ok(hash) => {
                    TelegramMessage::new(&bot, msg.chat.id, msg.thread_id)
                        .title("Withdraw")
                        .field("You have withdrawn", format!("{} XEL", format_xelis(amount)), false)
                        .field("Transaction", InlineCode::new(&hash.to_string()), false)
                        .send().await?;
                },
                Err(e) => {
                    bot.send_message(msg.chat.id, format!("An error occured while withdrawing: {}", e)).await?;
                }
            };
        },
        TelegramCommand::Tip { amount } => {
            let from = msg.from.as_ref().ok_or(TelegramError::NoUser)?;
            let dm = from.id;
            let amount = match from_xelis(amount.to_string()) {
                Some(amount) => amount,
                None => {
                    debug!("Invalid amount");
                    bot.send_message(dm, "An error occured while tipping: Invalid amount").await?;
                    return Ok(());
                }
            };

            let to = msg.reply_to_message().and_then(|m| m.from.as_ref()).ok_or(TelegramError::NoUser)?;

            if to.is_bot || to.is_anonymous() || to.is_channel() {
                debug!("Invalid user");
                bot.send_message(dm, "An error occured while tipping: Invalid user").await?;
                return Ok(());
            }

            match state.transfer(&UserApplication::Telegram(from.id.0), &UserApplication::Telegram(to.id.0), amount).await {
                Ok(()) => {
                    
                    debug!("Tipped {} XEL to {} (chat id: {}, thread: {:?})", format_xelis(amount), to.id, msg.chat.id, thread_id);
                    TelegramMessage::new(&bot, msg.chat.id, thread_id)
                        .title("Tip")
                        .field("You have tipped", format!("{} XEL", format_xelis(amount)), false)
                        .field("To", format!("{} ({})", to.username.as_ref().unwrap_or(&to.first_name), to.id), false)
                        .send().await?;
                },
                Err(e) => {
                    debug!("An error occured while tipping: {}", e);
                    bot.send_message(dm, format!("An error occured while tipping: {}", e)).await?;
                }
            };
        }
    }

    Ok(())
}
