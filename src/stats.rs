use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use clap::Args;
use log::{error, info, warn};
use poise::serenity_prelude::{
    ChannelId, ChannelType, CreateChannel, EditChannel, GuildChannel, GuildId, Http,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use xelis_common::{api::daemon::GetInfoResult, config::COIN_VALUE};

use crate::service::{WalletService, XelisStatsSnapshot};

const DEFAULT_PRICE_URL: &str = "https://api.coinpaprika.com/v1/tickers/xel-xelis";
const DEFAULT_CATEGORY_TITLE: &str = "XELIS STATS";
const DEFAULT_STATE_PATH: &str = "xelis-stats-channels.json";
const DEFAULT_REFRESH_INTERVAL: &str = "5m";

#[derive(Debug, Clone, Args)]
pub struct StatsConfig {
    /// Discord guild id where XELIS stats voice channels should be managed
    #[clap(long = "stats-guild-id")]
    pub guild_id: Option<u64>,
    /// Discord category id for stats channels. If omitted, the bot finds or creates one.
    #[clap(long = "stats-category-id")]
    pub category_id: Option<u64>,
    /// Discord category title for stats channels
    #[clap(long = "stats-category-title", default_value_t = default_category_title())]
    pub category_title: String,
    /// Price API endpoint used by stats channels
    #[clap(long = "stats-price-url", default_value_t = default_price_url())]
    pub price_url: String,
    /// File used to persist Discord stats channel ids
    #[clap(long = "stats-state-path", default_value = DEFAULT_STATE_PATH)]
    pub state_path: PathBuf,
    /// Refresh interval for stats channels, for example 400s, 5m, or 1h 30m
    #[clap(long = "stats-refresh-interval", value_parser = humantime::parse_duration, default_value = DEFAULT_REFRESH_INTERVAL)]
    pub refresh_interval: Duration,
}

impl StatsConfig {
    pub fn enabled(&self) -> bool {
        self.guild_id.is_some()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChannelState {
    category_id: Option<u64>,
    channels: BTreeMap<String, u64>,
}

impl ChannelState {
    fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("Error while parsing {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("Error while reading {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .with_context(|| format!("Error while creating {}", parent.display()))?;
        }

        let contents = serde_json::to_string_pretty(self)?;
        fs::write(path, contents).with_context(|| format!("Error while writing {}", path.display()))
    }

    fn channel_id(&self, label: &str) -> Option<ChannelId> {
        self.channels
            .get(label)
            .copied()
            .filter(|id| *id != 0)
            .map(ChannelId::new)
    }

    fn set_channel_id(&mut self, label: &str, channel_id: ChannelId) {
        self.channels.insert(label.to_string(), channel_id.get());
    }
}

struct StatsUpdater {
    http: Arc<Http>,
    service: WalletService,
    http_client: Client,
    guild_id: u64,
    config: StatsConfig,
}

impl StatsUpdater {
    fn new(http: Arc<Http>, service: WalletService, guild_id: u64, config: StatsConfig) -> Self {
        Self {
            http,
            service,
            http_client: Client::new(),
            guild_id,
            config,
        }
    }

    async fn run(mut self) {
        let mut state = match ChannelState::load(&self.config.state_path) {
            Ok(state) => state,
            Err(e) => {
                error!("Error while loading XELIS stats channel state: {:?}", e);
                ChannelState::default()
            }
        };

        loop {
            if let Err(e) = self.sync_once(&mut state).await {
                error!("Error while updating XELIS stats channels: {:?}", e);
            }

            tokio::time::sleep(self.config.refresh_interval.max(Duration::from_secs(1))).await;
        }
    }

    async fn sync_once(&mut self, state: &mut ChannelState) -> Result<()> {
        let guild_id = GuildId::new(self.guild_id);
        let channels = guild_id.channels(self.http.as_ref()).await?;
        let (category_id, mut state_changed) =
            self.ensure_category(guild_id, &channels, state).await?;
        let stats = self.fetch_stats().await;

        for (label, name) in stats.channel_names() {
            if self
                .update_or_create_channel(guild_id, &channels, category_id, state, label, &name)
                .await?
            {
                state_changed = true;
            }
        }

        if state_changed {
            state.save(&self.config.state_path)?;
        }

        Ok(())
    }

    async fn ensure_category(
        &self,
        guild_id: GuildId,
        channels: &HashMap<ChannelId, GuildChannel>,
        state: &mut ChannelState,
    ) -> Result<(ChannelId, bool)> {
        let configured_category_id = self
            .config
            .category_id
            .filter(|id| *id != 0)
            .map(ChannelId::new);
        let stored_category_id = state.category_id.filter(|id| *id != 0).map(ChannelId::new);

        if let Some(category_id) = configured_category_id.or(stored_category_id) {
            if let Some(category) = channels.get(&category_id) {
                if category.kind != ChannelType::Category {
                    bail!(
                        "Stats category id {} is not a Discord category",
                        category_id
                    );
                }

                if category.name != self.config.category_title {
                    category_id
                        .edit(
                            self.http.as_ref(),
                            EditChannel::new().name(&self.config.category_title),
                        )
                        .await?;
                    info!(
                        "XELIS stats category name set to {}",
                        self.config.category_title
                    );
                }

                let changed = state.category_id != Some(category_id.get());
                state.category_id = Some(category_id.get());
                return Ok((category_id, changed));
            }

            if configured_category_id.is_some() {
                bail!("Configured stats category id {} was not found", category_id);
            }

            warn!(
                "Stored XELIS stats category {} no longer exists; creating a new one",
                category_id
            );
        }

        if let Some(category) = channels.values().find(|channel| {
            channel.kind == ChannelType::Category && channel.name == self.config.category_title
        }) {
            let changed = state.category_id != Some(category.id.get());
            state.category_id = Some(category.id.get());
            return Ok((category.id, changed));
        }

        let category = guild_id
            .create_channel(
                self.http.as_ref(),
                CreateChannel::new(&self.config.category_title).kind(ChannelType::Category),
            )
            .await?;
        state.category_id = Some(category.id.get());
        info!(
            "Created XELIS stats category {}",
            self.config.category_title
        );

        Ok((category.id, true))
    }

    async fn update_or_create_channel(
        &self,
        guild_id: GuildId,
        channels: &HashMap<ChannelId, GuildChannel>,
        category_id: ChannelId,
        state: &mut ChannelState,
        label: &str,
        new_name: &str,
    ) -> Result<bool> {
        if let Some(channel_id) = state.channel_id(label) {
            if let Some(channel) = channels.get(&channel_id) {
                self.edit_channel_if_needed(channel, category_id, new_name)
                    .await?;
                return Ok(false);
            }

            warn!(
                "Stored XELIS stats channel {} for {} no longer exists",
                channel_id, label
            );
        }

        if let Some(channel) = find_existing_channel(channels, category_id, label) {
            self.edit_channel_if_needed(channel, category_id, new_name)
                .await?;
            state.set_channel_id(label, channel.id);
            return Ok(true);
        }

        let channel = guild_id
            .create_channel(
                self.http.as_ref(),
                CreateChannel::new(new_name)
                    .kind(ChannelType::Voice)
                    .category(category_id),
            )
            .await?;
        state.set_channel_id(label, channel.id);
        info!("Created XELIS stats channel {}", new_name);

        Ok(true)
    }

    async fn edit_channel_if_needed(
        &self,
        channel: &GuildChannel,
        category_id: ChannelId,
        new_name: &str,
    ) -> Result<()> {
        if channel.name == new_name && channel.parent_id == Some(category_id) {
            return Ok(());
        }

        channel
            .id
            .edit(
                self.http.as_ref(),
                EditChannel::new().name(new_name).category(category_id),
            )
            .await?;
        info!(
            "Updated XELIS stats channel {} to {}",
            channel.name, new_name
        );

        Ok(())
    }

    async fn fetch_stats(&self) -> StatsSnapshot {
        let xelis = match self.service.get_xelis_stats_snapshot().await {
            Ok(value) => Some(value),
            Err(e) => {
                warn!("Error while fetching XELIS stats from wallet daemon API: {:?}", e);
                None
            }
        };

        let price = match self.fetch_price().await {
            Ok(value) => Some(value),
            Err(e) => {
                warn!("Error while fetching XELIS price: {:?}", e);
                None
            }
        };

        StatsSnapshot {
            xelis,
            price,
        }
    }

    async fn fetch_price(&self) -> Result<f64> {
        let value: serde_json::Value = self
            .http_client
            .get(&self.config.price_url)
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;

        match value.pointer("/quotes/USD/price") {
            Some(serde_json::Value::Number(value)) => value
                .as_f64()
                .context("price was not representable as f64"),
            Some(serde_json::Value::String(value)) => value
                .parse()
                .context("price string was not a number"),
            _ => bail!("price was missing from Coinpaprika response"),
        }
    }
}

struct StatsSnapshot {
    xelis: Option<XelisStatsSnapshot>,
    price: Option<f64>,
}

impl StatsSnapshot {
    fn channel_names(&self) -> Vec<(&'static str, String)> {
        let info = self.xelis.as_ref().map(|snapshot| &snapshot.info);
        let difficulty = self.xelis.as_ref().map(|snapshot| &snapshot.difficulty);

        let network = info
            .map(|info| info.network.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        let block_time = info
            .map(|info| format_seconds(info.average_block_time))
            .unwrap_or_else(|| "N/A".to_string());
        let block_reward = info
            .map(|info| format_block_reward(info.block_reward))
            .unwrap_or_else(|| "N/A".to_string());
        let maximum_supply = info
            .map(|info| format_max_supply(info.maximum_supply))
            .unwrap_or_else(|| "N/A".to_string());
        let circulating_supply = info
            .map(|info| format_circulating_supply(info.circulating_supply))
            .unwrap_or_else(|| "N/A".to_string());
        let net_hash = difficulty
            .map(|difficulty| difficulty.hashrate_formatted.clone())
            .unwrap_or_else(|| "N/A".to_string());
        let coins_mined = info
            .and_then(coins_mined_percentage)
            .unwrap_or_else(|| "N/A".to_string());
        let price = self
            .price
            .map(format_price)
            .unwrap_or_else(|| "N/A".to_string());
        let market_cap = info
            .and_then(|info| market_cap(info, self.price))
            .unwrap_or_else(|| "N/A".to_string());

        vec![
            ("Network:", channel_name("Network:", network)),
            ("Block Time:", channel_name("Block Time:", block_time)),
            ("Block Reward:", channel_name("Block Reward:", block_reward)),
            ("Max Supply:", channel_name("Max Supply:", maximum_supply)),
            (
                "Circ Supply:",
                channel_name("Circ Supply:", circulating_supply),
            ),
            ("Net Hash:", channel_name("Net Hash:", net_hash)),
            ("Coins Mined:", channel_name("Coins Mined:", coins_mined)),
            ("Price:", channel_name("Price:", price)),
            ("Mcap:", channel_name("Mcap:", market_cap)),
        ]
    }
}

pub fn spawn_stats_updater(http: Arc<Http>, service: WalletService, config: StatsConfig) {
    let Some(guild_id) = config.guild_id else {
        return;
    };

    tokio::spawn(async move {
        info!(
            "Starting XELIS stats updater for guild {} every {}",
            guild_id,
            humantime::format_duration(config.refresh_interval.max(Duration::from_secs(1)))
        );
        StatsUpdater::new(http, service, guild_id, config).run().await;
    });
}

pub fn default_price_url() -> String {
    DEFAULT_PRICE_URL.to_string()
}

pub fn default_category_title() -> String {
    DEFAULT_CATEGORY_TITLE.to_string()
}

fn find_existing_channel<'a>(
    channels: &'a HashMap<ChannelId, GuildChannel>,
    category_id: ChannelId,
    label: &str,
) -> Option<&'a GuildChannel> {
    channels.values().find(|channel| {
        channel.kind == ChannelType::Voice
            && channel.parent_id == Some(category_id)
            && channel.name.starts_with(label)
    })
}

fn channel_name(label: &str, value: String) -> String {
    let name = format!("{} {}", label, value);
    if name.len() <= 100 {
        name
    } else {
        name[0..100].to_string()
    }
}

fn format_seconds(milliseconds: u64) -> String {
    format!("{:.0}s avg", milliseconds as f64 / 1_000.0)
}

fn format_block_reward(atomic_units: u64) -> String {
    format!("{:.4} XEL", atomic_units as f64 / COIN_VALUE as f64)
}

fn format_max_supply(atomic_units: u64) -> String {
    format!("{:.1}M XEL", atomic_units as f64 / COIN_VALUE as f64 / 1_000_000.0)
}

fn format_circulating_supply(atomic_units: u64) -> String {
    format!("{:.0} XEL", atomic_units as f64 / COIN_VALUE as f64)
}

fn coins_mined_percentage(info: &GetInfoResult) -> Option<String> {
    if info.maximum_supply == 0 {
        return None;
    }

    Some(format!(
        "{:.2}%",
        info.circulating_supply as f64 / info.maximum_supply as f64 * 100.0
    ))
}

fn format_price(price: f64) -> String {
    format!("${:.4}", price)
}

fn market_cap(info: &GetInfoResult, price: Option<f64>) -> Option<String> {
    let circulating_supply = info.circulating_supply as f64 / COIN_VALUE as f64;
    Some(format!(
        "${}",
        format_with_commas((circulating_supply * price?).round() as u64)
    ))
}

fn format_with_commas(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    let first_group_len = match digits.len() % 3 {
        0 => 3,
        len => len,
    };

    formatted.push_str(&digits[..first_group_len]);
    let mut index = first_group_len;
    while index < digits.len() {
        formatted.push(',');
        formatted.push_str(&digits[index..index + 3]);
        index += 3;
    }

    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_market_cap_with_commas() {
        assert_eq!(format_with_commas(1), "1");
        assert_eq!(format_with_commas(1_234), "1,234");
        assert_eq!(format_with_commas(1_234_567), "1,234,567");
    }

    #[test]
    fn builds_discord_channel_name() {
        assert_eq!(
            channel_name("Price:", "$1.2345".to_string()),
            "Price: $1.2345"
        );
    }
}
